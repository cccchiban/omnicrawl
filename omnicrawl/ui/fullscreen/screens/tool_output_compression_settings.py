"""工具输出压缩设置（可内嵌设置页右侧的 Pane）。

页面结构：启用开关 + 当前压缩模型说明 + 内嵌双列模型选择器（``selection_only``：
只“选择”压缩模型，不切换主模型）+ 4 个预算输入 + 保存/取消按钮。

保存语义与 advisor/vision 对齐：写盘 ``config.toml [tool_output_compression]``
段，再调用 ``apply_configuration``（host 负责同步 agent.config）；任一步失败回滚磁盘。
启用但未选择模型时拒绝保存。
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, Vertical, VerticalScroll
from textual.widgets import Button, Input, Select, Static

from ....config.features.tool_output_compression import (
    ToolOutputCompressionConfig,
    ToolOutputCompressionConfigError,
    DEFAULT_THINKING_EFFORT,
    THINKING_EFFORT_OPTIONS,
    load_tool_output_compression_config,
    save_tool_output_compression_config,
)
from ..terminal.theme import terminal_css, terminal_select_css
from .model_picker import ModelPickerPane, ModelPickerResult
from .panes import SettingsPane

_THINKING_LABELS = {
    "low": "低",
    "medium": "中",
    "high": "高",
    "xhigh": "超高",
    "max": "最大",
}

_PANE_CSS = """
#toc-form { height: 1fr; }
#toc-model { height: 1; margin-bottom: 1; color: $terminal-text-secondary; }
.toc-label { height: 1; color: $terminal-text-muted; }
.toc-control { margin-bottom: 1; }
#toc-picker-wrap {
    /* 高度优先给模型选择器：矮窗口下 min-height 保证仍有可浏览行。 */
    height: 1fr;
    min-height: 16;
    border: round $terminal-border;
    background: $terminal-background;
    margin-bottom: 1;
}
#toc-status { height: 1; color: $terminal-white; margin-bottom: 1; }
#toc-actions { height: 3; align-horizontal: right; }
"""


@dataclass(frozen=True)
class ToolOutputCompressionSettingsResult:
    """工具输出压缩设置保存结果。"""

    configuration: ToolOutputCompressionConfig
    config_path: Path | None


class ToolOutputCompressionSettingsPane(SettingsPane):
    """工具输出压缩二级面板：开关 + 预算 + 内嵌模型选择器 + 保存。"""

    BINDINGS = [
        Binding("escape", "cancel", "返回", priority=True),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    DEFAULT_CSS = terminal_css(_PANE_CSS + terminal_select_css())

    def __init__(
        self,
        agent: Any,
        config_path: str | Path | None = None,
        *,
        apply_configuration: Callable[[ToolOutputCompressionConfig], None] | None = None,
        configuration: ToolOutputCompressionConfig | None = None,
    ) -> None:
        super().__init__(agent=agent)
        self._config_path = Path(config_path) if config_path is not None else None
        self._apply_configuration = apply_configuration
        loaded = configuration or (
            load_tool_output_compression_config(self._config_path)
            if self._config_path is not None
            else ToolOutputCompressionConfig()
        )
        self._previous_configuration = loaded
        self._enabled = loaded.enabled
        self._model_key = loaded.model_key
        self._thinking_enabled = loaded.thinking_enabled
        self._thinking_effort = loaded.reasoning_effort
        if self._thinking_effort not in THINKING_EFFORT_OPTIONS:
            self._thinking_effort = DEFAULT_THINKING_EFFORT
        self._picker: Optional[ModelPickerPane] = None
        self._status = "选择压缩模型、调整开关后按 Ctrl+S 保存。"

    def compose_pane(self) -> ComposeResult:
        # 可滚动表单区：字段 + 当前模型说明 + 内嵌模型选择器；状态与按钮栏固定在下。
        with VerticalScroll(id="toc-form"):
            yield Static("启用", classes="toc-label")
            yield Select(
                [("停用", False), ("启用", True)],
                value=self._enabled,
                allow_blank=False,
                id="toc-enabled",
                classes="toc-control choice-select",
            )
            yield Static("作用域：bash / powershell / git", classes="toc-label")
            yield Static("思考", classes="toc-label")
            yield Select(
                [("关闭", False), ("开启", True)],
                value=self._thinking_enabled,
                allow_blank=False,
                id="toc-thinking",
                classes="toc-control choice-select",
            )
            yield Static("思考深度", classes="toc-label")
            yield Select(
                [
                    (_THINKING_LABELS.get(option, option), option)
                    for option in THINKING_EFFORT_OPTIONS
                ],
                value=self._thinking_effort,
                allow_blank=False,
                id="toc-effort",
                classes="toc-control choice-select",
            )
            for label, widget_id, value in self._numeric_rows():
                yield Static(label, classes="toc-label")
                yield Input(str(value), id=widget_id, classes="toc-control")
            yield Static(self._current_text(), id="toc-model")
            with Container(id="toc-picker-wrap"):
                self._picker = ModelPickerPane(
                    self._agent,
                    selection_only=True,
                    refresh_on_open=False,
                    current_override=self._model_key or None,
                )
                self._picker.bind_pane_events(
                    on_back=self.request_back,
                    on_commit=self._receive_model,
                )
                yield self._picker
        with Vertical(id="toc-footer"):
            yield Static(self._status, id="toc-status")
            with Horizontal(id="toc-actions"):
                yield Static("", classes="pane-save-hint")
                yield Button("取消", id="toc-cancel")
                yield Button("保存", variant="primary", id="toc-save")

    def refresh_pane(self) -> None:
        if not self._can_refresh():
            return
        # Select/Input 是用户编辑控件，值由用户操作维护；刷新只更新派生文本。
        self.query_one("#toc-model", Static).update(self._current_text())
        self.query_one("#toc-status", Static).update(self._status)

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "toc-save":
            self.action_save()
        elif event.button.id == "toc-cancel":
            self.action_exit()

    def activate(self) -> None:
        self.query_one("#toc-enabled", Select).focus()

    def action_cancel(self) -> None:
        self.request_back()

    def action_exit(self) -> None:
        """“取消”按钮：直接关闭整个设置面板。"""
        self.request_exit()

    def action_save(self) -> None:
        enabled = bool(self.query_one("#toc-enabled", Select).value)
        if enabled and not (self._model_key or "").strip():
            self._status = "启用压缩前请先在下方选择压缩模型。"
            self._render_status()
            return
        thinking_value = self.query_one("#toc-thinking", Select).value
        self._thinking_enabled = bool(thinking_value)
        effort_value = self.query_one("#toc-effort", Select).value
        self._thinking_effort = (
            str(effort_value) if effort_value is not None else DEFAULT_THINKING_EFFORT
        )
        old = self._previous_configuration
        try:
            configuration = ToolOutputCompressionConfig(
                enabled=enabled,
                model_key=(self._model_key or "").strip(),
                thinking_enabled=self._thinking_enabled,
                reasoning_effort=self._thinking_effort,
                min_chars=self._read_positive_int("toc-min-chars", "最小压缩字符数"),
                max_input_chars=self._read_positive_int(
                    "toc-max-input-chars", "单次压缩输入上限"
                ),
                max_output_chars=self._read_positive_int(
                    "toc-max-output-chars", "压缩结果上限"
                ),
                timeout_seconds=self._read_positive_int("toc-timeout", "压缩超时秒数"),
            )
            if self._config_path is not None:
                path: Path | None = save_tool_output_compression_config(
                    configuration, self._config_path
                )
            else:
                path = None
            if self._apply_configuration is not None:
                try:
                    self._apply_configuration(configuration)
                except Exception:
                    if self._config_path is not None:
                        save_tool_output_compression_config(old, self._config_path)
                    raise
        except (ToolOutputCompressionConfigError, OSError, RuntimeError, ValueError) as exc:
            self._status = f"工具输出压缩设置保存失败：{exc}"
            self._render_status()
            return
        self.flash_save_hint()
        self.commit(ToolOutputCompressionSettingsResult(configuration, path))

    def _receive_model(self, result: ModelPickerResult | None) -> None:
        """内嵌选择器 commit：接收压缩模型引用（不切换主模型）。"""
        if result is None:
            return
        self._model_key = result.model
        self._status = f"压缩模型已选择：{result.model}；按 Ctrl+S 保存。"
        self.refresh_pane()

    def _numeric_rows(self) -> tuple[tuple[str, str, int], ...]:
        loaded = self._previous_configuration
        return (
            ("最小压缩字符数（模型可见文本短于此值不压缩）", "toc-min-chars", loaded.min_chars),
            ("单次压缩输入上限（字符）", "toc-max-input-chars", loaded.max_input_chars),
            ("压缩结果上限（字符）", "toc-max-output-chars", loaded.max_output_chars),
            ("单条压缩超时（秒）", "toc-timeout", loaded.timeout_seconds),
        )

    def _read_positive_int(self, widget_id: str, label: str) -> int:
        raw = self.query_one(f"#{widget_id}", Input).value.strip()
        try:
            value = int(raw)
        except ValueError as exc:
            raise ValueError(f"{label}必须是正整数。") from exc
        if value <= 0:
            raise ValueError(f"{label}必须是正整数。")
        return value

    def _current_text(self) -> str:
        if not (self._model_key or "").strip():
            return "当前压缩模型：未选择（启用前请在下方选择模型）"
        thinking = (
            f"思考 {self._thinking_effort}" if self._thinking_enabled else "思考关闭"
        )
        return f"当前压缩模型：{self._model_key}（{thinking}）"

    def _render_status(self) -> None:
        if self.is_mounted:
            self.query_one("#toc-status", Static).update(self._status)


__all__ = ["ToolOutputCompressionSettingsResult", "ToolOutputCompressionSettingsPane"]
