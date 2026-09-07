"""顾问（advisor）设置（可内嵌设置页右侧的 Pane）。

页面结构：启用开关 + 推理档位 + 当前顾问模型说明 + 内嵌双列
ModelPickerPane（与主模型行同款，但 ``selection_only``：只“选择”，
不切换主模型，选择结果作为顾问模型候选暂存）+ 保存/取消按钮。

保存语义与 vision/run_guard 对齐：写盘 ``config.toml [advisor]`` 段，
再调用 ``apply_configuration``（host 负责同步 agent.config.advisor 并
重建工具表）；任一步失败回滚磁盘。启用但未选择模型时拒绝保存。
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, Vertical, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Select, Static

from ....config.features.advisor import (
    ADVISOR_EFFORT_OPTIONS,
    AdvisorConfig,
    AdvisorConfigError,
    DEFAULT_ADVISOR_EFFORT,
    load_advisor_config,
    save_advisor_config,
)
from ..terminal.theme import terminal_css, terminal_select_css
from .model_picker import ModelPickerPane, ModelPickerResult
from .panes import SettingsPane

_EFFORT_LABELS = {
    "none": "关闭",
    "low": "低",
    "medium": "中",
    "high": "高",
    "xhigh": "超高",
    "max": "最大",
}

_PANE_CSS = """
#advisor-form { height: 1fr; }
#advisor-fields { height: 3; margin-bottom: 1; }
.advisor-field { width: 1fr; height: 3; }
.advisor-field-label {
    width: 7;
    height: 3;
    content-align: left middle;
    color: $terminal-text-muted;
    margin-right: 1;
}
.advisor-field-select { width: 1fr; }
#advisor-current { height: 1; margin-bottom: 1; color: $terminal-text-secondary; }
#advisor-picker-wrap {
    /* 高度优先给模型选择器：1fr 占滚动区剩余全部空间，min-height 兜底保证
       矮窗口（设置页右侧高度受限）下双列列表仍有可浏览行；超出滚动视口
       的部分由外层 #advisor-form(VerticalScroll) 承接滚动。 */
    height: 1fr;
    min-height: 20;
    border: round $terminal-border;
    background: $terminal-background;
    margin-bottom: 1;
}
#advisor-footer { height: auto; }
#advisor-status { height: 1; color: $terminal-white; margin-bottom: 1; }
#advisor-actions { height: 3; align-horizontal: right; }
"""


@dataclass(frozen=True)
class AdvisorSettingsResult:
    """顾问设置保存结果。"""

    configuration: AdvisorConfig
    config_path: Path


class AdvisorSettingsPane(SettingsPane):
    """顾问二级面板：启用开关 + effort + 内嵌模型选择器 + 保存。"""

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
        apply_configuration: Callable[[AdvisorConfig], None] | None = None,
        configuration: AdvisorConfig | None = None,
    ) -> None:
        super().__init__(agent=agent)
        self._config_path = Path(config_path) if config_path is not None else None
        self._apply_configuration = apply_configuration
        loaded = configuration or (
            load_advisor_config(self._config_path)
            if self._config_path is not None
            else AdvisorConfig()
        )
        self._previous_configuration = loaded
        self._enabled = loaded.enabled
        self._effort = loaded.display_effort or DEFAULT_ADVISOR_EFFORT
        if self._effort not in ADVISOR_EFFORT_OPTIONS:
            self._effort = DEFAULT_ADVISOR_EFFORT
        self._model_key = loaded.model_key
        self._picker: Optional[ModelPickerPane] = None
        self._status = "选择模型、调整开关后按 Ctrl+S 保存。"

    def compose_pane(self) -> ComposeResult:
        # 可滚动表单区：顶部字段行 + 当前说明 + 内嵌模型选择器。内容超高
        # 时由 VerticalScroll 提供滚动（滚轮/PageUp/PageDown）。状态与按钮
        # 栏固定在下（不随内容滚动，始终可达）。
        with VerticalScroll(id="advisor-form"):
            # 启用 / 推理档位 并排一行，避免把模型选择器挤到视口底部。
            with Horizontal(id="advisor-fields"):
                with Horizontal(classes="advisor-field"):
                    yield Static("启用", classes="advisor-field-label")
                    yield Select(
                        [("停用", False), ("启用", True)],
                        value=self._enabled,
                        allow_blank=False,
                        id="advisor-enabled",
                        classes="advisor-field-select choice-select",
                    )
                with Horizontal(classes="advisor-field"):
                    yield Static("effort", classes="advisor-field-label")
                    yield Select(
                        [
                            (_EFFORT_LABELS.get(opt, opt), opt)
                            for opt in ADVISOR_EFFORT_OPTIONS
                        ],
                        value=self._effort,
                        allow_blank=False,
                        id="advisor-effort",
                        classes="advisor-field-select choice-select",
                    )
            yield Static(self._current_text(), id="advisor-current")
            with Container(id="advisor-picker-wrap"):
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
        with Vertical(id="advisor-footer"):
            yield Static(self._status, id="advisor-status")
            with Horizontal(id="advisor-actions"):
                yield Static("", classes="pane-save-hint")
                yield Button("取消", id="advisor-cancel")
                yield Button("保存", variant="primary", id="advisor-save")

    def refresh_pane(self) -> None:
        if not self._can_refresh():
            return
        # Select 是用户编辑控件，值由用户操作维护；刷新只更新派生文本。
        self.query_one("#advisor-current", Static).update(self._current_text())
        self.query_one("#advisor-status", Static).update(self._status)

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "advisor-save":
            self.action_save()
        elif event.button.id == "advisor-cancel":
            self.action_exit()

    def activate(self) -> None:
        self.query_one("#advisor-enabled", Select).focus()

    def action_cancel(self) -> None:
        self.request_back()

    def action_exit(self) -> None:
        """“取消”按钮：直接关闭整个设置面板。"""
        self.request_exit()

    def action_save(self) -> None:
        self._enabled = bool(self.query_one("#advisor-enabled", Select).value)
        effort_value = self.query_one("#advisor-effort", Select).value
        self._effort = str(effort_value) if effort_value is not None else DEFAULT_ADVISOR_EFFORT
        if self._enabled and not (self._model_key or "").strip():
            self._status = "启用顾问前请先在下方选择顾问模型。"
            self._render_status()
            return
        old = self._previous_configuration
        try:
            configuration = AdvisorConfig(
                enabled=self._enabled,
                model_key=(self._model_key or "").strip(),
                effort=self._effort,
                disabled_for_models=old.disabled_for_models,
            )
            if self._config_path is not None:
                path = save_advisor_config(configuration, self._config_path)
            else:
                path = self._config_path
            if self._apply_configuration is not None:
                try:
                    self._apply_configuration(configuration)
                except Exception:
                    if self._config_path is not None:
                        save_advisor_config(old, self._config_path)
                    raise
        except (AdvisorConfigError, OSError, RuntimeError) as exc:
            self._status = f"顾问设置保存失败：{exc}"
            self._render_status()
            return
        self.flash_save_hint()
        self.commit(AdvisorSettingsResult(configuration, path))

    def _receive_model(self, result: ModelPickerResult | None) -> None:
        """内嵌选择器 commit：暂存顾问模型引用（不切换主模型）。"""
        if result is None:
            return
        self._model_key = result.model
        self._status = f"顾问模型已选择：{result.model}；按 Ctrl+S 保存。"
        self.refresh_pane()

    def _current_text(self) -> str:
        if not (self._model_key or "").strip():
            return "当前顾问：未选择（启用前必须先在下方选择模型）"
        return f"当前顾问：{self._model_key}（effort={self._effort}）"

    def _render_status(self) -> None:
        if self.is_mounted:
            self.query_one("#advisor-status", Static).update(self._status)


class AdvisorSettingsScreen(ModalScreen[Optional[AdvisorSettingsResult]]):
    """整屏薄壳：内嵌 AdvisorSettingsPane，保存 dismiss 结果、Esc 取消。

    供独立入口（如导航层）使用；设置页右侧直接挂载 Pane 时不经过本薄壳。
    """

    BINDINGS = [
        Binding("escape", "cancel", "取消"),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    CSS = terminal_css(
        """
    AdvisorSettingsScreen { align: center middle; background: $terminal-overlay; }
    #advisor-settings-dialog { width: 110; max-width: 96%; height: 40; max-height: 95%; padding: 1 2; border: round $terminal-border-strong; background: $terminal-surface; }
    #advisor-settings-title { height: 1; margin-bottom: 1; color: $terminal-white; text-style: bold; }
    """
        + _PANE_CSS
        + terminal_select_css()
    )

    def __init__(
        self,
        agent: Any,
        config_path: str | Path,
        *,
        apply_configuration: Callable[[AdvisorConfig], None] | None = None,
        configuration: AdvisorConfig | None = None,
    ) -> None:
        super().__init__()
        self._agent = agent
        self._config_path = Path(config_path)
        self._apply_configuration = apply_configuration
        self._configuration = configuration
        self._pane: Optional[AdvisorSettingsPane] = None

    # 状态转发：既有测试与导航层读取整屏对象上的 _status/_enabled/_model_key。
    @property
    def _status(self) -> str:
        return self._pane._status if self._pane is not None else ""

    @_status.setter
    def _status(self, value: str) -> None:
        if self._pane is not None:
            self._pane._status = value

    @property
    def _enabled(self) -> bool:
        return self._pane._enabled if self._pane is not None else False

    @property
    def _model_key(self) -> str:
        return self._pane._model_key if self._pane is not None else ""

    def compose(self) -> ComposeResult:
        with Container(id="advisor-settings-dialog"):
            yield Static("顾问设置", id="advisor-settings-title")
            self._pane = AdvisorSettingsPane(
                self._agent,
                self._config_path,
                apply_configuration=self._apply_configuration,
                configuration=self._configuration,
            )
            self._pane.bind_pane_events(
                on_back=lambda: self.dismiss(None),
                on_commit=lambda result: self.dismiss(result),
                on_exit=lambda: self.dismiss(None),
            )
            yield self._pane

    def on_mount(self) -> None:
        if self._pane is not None:
            self._pane.activate()

    def action_cancel(self) -> None:
        self.dismiss(None)

    def action_save(self) -> None:
        if self._pane is not None:
            self._pane.action_save()


__all__ = ["AdvisorSettingsResult", "AdvisorSettingsPane", "AdvisorSettingsScreen"]
