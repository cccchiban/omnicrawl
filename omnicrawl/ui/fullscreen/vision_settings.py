"""视觉模型代理的设置界面。"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Static

from ...config.llm import ActiveModelRef
from ...config.vision import (
    VisionConfigError,
    VisionConfiguration,
    load_vision_configuration,
    save_vision_configuration,
)
from .model_picker import ModelPickerResult, ModelPickerScreen
from .theme import terminal_css


@dataclass(frozen=True)
class VisionSettingsResult:
    """视觉设置保存结果。"""

    configuration: VisionConfiguration
    config_path: Path


class VisionSettingsScreen(ModalScreen[Optional[VisionSettingsResult]]):
    """管理视觉代理开关和有序故障转移模型列表。"""

    BINDINGS = [
        Binding("escape", "cancel", "返回"),
        Binding("up", "move_up", "上一项", priority=True),
        Binding("down", "move_down", "下一项", priority=True),
        Binding("space", "toggle", "启用/停用", priority=True),
        Binding("a", "add", "添加视觉模型"),
        Binding("d", "delete", "删除视觉模型"),
        Binding("ctrl+up", "move_priority_up", "提高优先级", priority=True),
        Binding("ctrl+down", "move_priority_down", "降低优先级", priority=True),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    CSS = terminal_css("""
    VisionSettingsScreen {
        align: center middle;
        background: $terminal-overlay;
    }
    #vision-settings-dialog {
        width: 96;
        max-width: 96%;
        height: 30;
        max-height: 92%;
        padding: 1 2;
        border: round $terminal-border-strong;
        background: $terminal-surface;
    }
    #vision-settings-title {
        height: 1;
        margin-bottom: 1;
        color: $terminal-white;
        text-style: bold;
    }
    #vision-settings-enabled {
        height: 2;
        padding: 0 1;
        color: $terminal-text;
        background: $terminal-blue-soft;
    }
    #vision-settings-list {
        height: 1fr;
        margin-top: 1;
        border: round $terminal-border;
        background: $terminal-background;
        padding: 0 1;
    }
    .vision-model-row {
        height: 2;
        padding: 0 1;
        color: $terminal-text-secondary;
    }
    .vision-model-row.selected {
        color: $terminal-amber;
        text-style: bold;
    }
    #vision-settings-status {
        height: 2;
        margin-top: 1;
        color: $terminal-white;
    }
    #vision-settings-help {
        height: 2;
        color: $terminal-white;
    }
    """)

    def __init__(
        self,
        agent: Any,
        config_path: str | Path,
        *,
        apply_configuration: Callable[[VisionConfiguration], None] | None = None,
    ) -> None:
        super().__init__()
        self._agent = agent
        self._config_path = Path(config_path)
        self._apply_configuration = apply_configuration
        configuration = load_vision_configuration(self._config_path)
        self._previous_configuration = configuration
        self._enabled = configuration.enabled
        self._models = list(configuration.models)
        self._selected = 0
        self._rebuild_seq = 0
        self._status = "列表顺序就是故障转移顺序；按 A 从现有模型目录添加。"

    def compose(self) -> ComposeResult:
        with Container(id="vision-settings-dialog"):
            yield Static("视觉模型代理", id="vision-settings-title")
            yield Static(self._enabled_text(), id="vision-settings-enabled")
            with VerticalScroll(id="vision-settings-list"):
                if self._models:
                    for index, ref in enumerate(self._models):
                        yield Static(
                            self._row_text(index, ref),
                            id=f"vision-model-row-{index}-0",
                            classes=(
                                "vision-model-row selected"
                                if index == self._selected
                                else "vision-model-row"
                            ),
                        )
                else:
                    yield Static(
                        "尚未添加视觉模型，请按 A 从现有渠道/模型目录选择。",
                        id="vision-model-row-0-0",
                        classes="vision-model-row selected",
                    )
            yield Static(self._status, id="vision-settings-status")
            yield Static(
                "↑↓ 选择  空格启用/停用  A 添加  D 删除  Ctrl+↑↓ 调整故障转移顺序  Ctrl+S 保存  Esc 返回",
                id="vision-settings-help",
            )

    def on_mount(self) -> None:
        self.call_after_refresh(self._refresh_view)

    def action_cancel(self) -> None:
        self.dismiss(None)

    def action_move_up(self) -> None:
        if not self._models:
            return
        self._selected = (self._selected - 1) % len(self._models)
        self._refresh_view()

    def action_move_down(self) -> None:
        if not self._models:
            return
        self._selected = (self._selected + 1) % len(self._models)
        self._refresh_view()

    def action_toggle(self) -> None:
        self._enabled = not self._enabled
        self._status = f"视觉模型代理已{'启用' if self._enabled else '停用'}；按 Ctrl+S 保存。"
        self._refresh_view()

    def action_add(self) -> None:
        self.app.push_screen(
            ModelPickerScreen(
                self._agent,
                refresh_on_open=True,
                selection_only=True,
            ),
            self._receive_model_selection,
        )

    def action_delete(self) -> None:
        if not self._models:
            self._status = "当前没有可删除的视觉模型。"
            self._render_status()
            return
        removed = self._models.pop(self._selected)
        self._selected = min(self._selected, max(0, len(self._models) - 1))
        self._status = f"已移除视觉模型“{_model_ref_label(removed)}”；按 Ctrl+S 保存。"
        self._refresh_view()

    def action_move_priority_up(self) -> None:
        if self._selected <= 0 or not self._models:
            return
        self._models[self._selected - 1], self._models[self._selected] = (
            self._models[self._selected],
            self._models[self._selected - 1],
        )
        self._selected -= 1
        self._status = "已提高故障转移优先级；按 Ctrl+S 保存。"
        self._refresh_view()

    def action_move_priority_down(self) -> None:
        if self._selected >= len(self._models) - 1:
            return
        self._models[self._selected + 1], self._models[self._selected] = (
            self._models[self._selected],
            self._models[self._selected + 1],
        )
        self._selected += 1
        self._status = "已降低故障转移优先级；按 Ctrl+S 保存。"
        self._refresh_view()

    def action_save(self) -> None:
        if self._enabled and not self._models:
            self._status = "启用视觉模型代理前，至少添加一个视觉模型。"
            self._render_status()
            return
        try:
            configuration = VisionConfiguration(
                enabled=self._enabled,
                models=tuple(self._models),
            )
            path = save_vision_configuration(configuration, self._config_path)
            if self._apply_configuration is not None:
                try:
                    self._apply_configuration(configuration)
                except Exception:
                    save_vision_configuration(self._previous_configuration, self._config_path)
                    raise
        except (VisionConfigError, OSError, RuntimeError) as exc:
            self._status = f"视觉设置保存失败：{exc}"
            self._render_status()
            return
        self.dismiss(VisionSettingsResult(configuration, path))

    def _receive_model_selection(self, result: ModelPickerResult | None) -> None:
        if result is None:
            return
        ref = _model_ref_from_picker(result)
        if ref is None:
            self._status = "无法保存该模型引用；请从已配置渠道或模型目录中选择。"
            self._render_status()
            return
        if any(item.to_dict() == ref.to_dict() for item in self._models):
            self._status = f"视觉模型“{_model_ref_label(ref)}”已经在列表中。"
            self._render_status()
            return
        self._models.append(ref)
        self._selected = len(self._models) - 1
        self._status = f"已添加“{_model_ref_label(ref)}”；按 Ctrl+S 保存。"
        self._refresh_view()

    def _enabled_text(self) -> str:
        state = "已启用" if self._enabled else "已停用"
        return f"视觉代理：{state}    已配置模型：{len(self._models)} 个"

    def _row_text(self, index: int, ref: ActiveModelRef) -> str:
        cursor = "›" if index == self._selected else " "
        return f"{cursor} [{index + 1}] {_model_ref_label(ref)}"

    def _refresh_view(self) -> None:
        if not self.is_mounted:
            return
        self.query_one("#vision-settings-enabled", Static).update(self._enabled_text())
        self._rebuild_rows()
        self._render_status()

    def _render_status(self) -> None:
        if self.is_mounted:
            self.query_one("#vision-settings-status", Static).update(self._status)

    def _rebuild_rows(self) -> None:
        container = self.query_one("#vision-settings-list", VerticalScroll)
        container.remove_children()
        self._rebuild_seq += 1
        if not self._models:
            container.mount(
                Static(
                    "尚未添加视觉模型，请按 A 从现有渠道/模型目录选择。",
                    id=f"vision-model-row-0-{self._rebuild_seq}",
                    classes="vision-model-row selected",
                )
            )
            return
        for index, ref in enumerate(self._models):
            container.mount(
                Static(
                    self._row_text(index, ref),
                    id=f"vision-model-row-{index}-{self._rebuild_seq}",
                    classes=(
                        "vision-model-row selected"
                        if index == self._selected
                        else "vision-model-row"
                    ),
                )
            )


def _model_ref_from_picker(result: ModelPickerResult) -> ActiveModelRef | None:
    if result.source == "custom" and result.key:
        return ActiveModelRef(source="custom", key=result.key)
    if result.source == "detected" and result.profile and result.model_id:
        return ActiveModelRef(
            source="detected",
            profile=result.profile,
            model_id=result.model_id,
            protocol=result.protocol,
        )
    return None


def _model_ref_label(ref: ActiveModelRef) -> str:
    if ref.source == "custom":
        return ref.key or "custom/unknown"
    if ref.profile and ref.model_id:
        return f"{ref.profile}/{ref.model_id}"
    return ref.model_id or ref.profile or "unknown"


__all__ = ["VisionSettingsResult", "VisionSettingsScreen"]
