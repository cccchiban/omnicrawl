"""持续运转与自动续跑设置（可内嵌右侧的 Pane + 整屏薄壳）。"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Input, Select, Static

from ....config.features.run_guard import (
    ContinueConfig,
    ReasoningGuardConfig,
    RunGuardConfig,
    RunGuardConfigError,
    load_run_guard_config,
    save_run_guard_config,
)
from ..terminal.theme import terminal_css, terminal_select_css
from .panes import SettingsPane

_PANE_CSS = """
#run-guard-form { height: 1fr; }
.run-guard-label { height: 1; color: $terminal-text-muted; }
.run-guard-control { height: 3; margin-bottom: 1; }
#run-guard-status { height: 2; color: $terminal-white; }
#run-guard-actions { height: 3; align-horizontal: right; }
"""


class RunGuardSettingsPane(SettingsPane):
    """运行护栏二级面板：编辑护栏/续跑配置，保存后返回左侧。"""

    BINDINGS = [
        Binding("escape", "cancel", "返回", priority=True),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    DEFAULT_CSS = terminal_css(_PANE_CSS + terminal_select_css())

    def __init__(
        self,
        config_path: str | Path | None = None,
        *,
        agent: Any | None = None,
        configuration: RunGuardConfig | None = None,
        apply_configuration: Any | None = None,
    ) -> None:
        super().__init__(agent=agent)
        self._config_path = Path(config_path) if config_path is not None else None
        self._configuration = configuration or (
            load_run_guard_config(self._config_path) if self._config_path is not None
            else RunGuardConfig()
        )
        self._apply_configuration = apply_configuration

    def compose_pane(self) -> ComposeResult:
        c = self._configuration
        g = c.guard
        cont = c.continuation
        with VerticalScroll(id="run-guard-form"):
            yield Static("持续运转功能总开关", classes="run-guard-label")
            yield Select(
                [("停用", False), ("启用", True)],
                value=c.enabled,
                allow_blank=False,
                id="run-guard-enabled",
                classes="run-guard-control choice-select",
            )
            yield Static("Reasoning Guard 开关", classes="run-guard-label")
            yield Select(
                [("停用", False), ("启用", True)],
                value=g.enabled,
                allow_blank=False,
                id="run-guard-guard-enabled",
                classes="run-guard-control choice-select",
            )
            for label, widget_id, value in (
                ("推理滑动窗口字符数", "run-guard-window", g.window_chars),
                ("重复子串长度", "run-guard-substr", g.substr_len),
                ("重复率阈值（0～1）", "run-guard-ratio", g.repeat_ratio),
                ("每多少个推理块检查一次", "run-guard-check", g.check_every),
                ("单次推理块数上限", "run-guard-blocks", g.max_blocks),
                ("单次推理字符数上限", "run-guard-chars", g.max_chars),
                ("Guard/白名单错误最大重试次数", "run-guard-retries", g.max_guard_retries),
            ):
                yield Static(label, classes="run-guard-label")
                yield Input(str(value), id=widget_id, classes="run-guard-control")
            yield Static("自动重试错误码（逗号分隔，可留空）", classes="run-guard-label")
            yield Input(
                ", ".join(g.auto_retry_errors),
                id="run-guard-errors",
                classes="run-guard-control",
            )
            yield Static("自动续跑开关", classes="run-guard-label")
            yield Select(
                [("停用", False), ("启用", True)],
                value=cont.enabled,
                allow_blank=False,
                id="run-guard-continue-enabled",
                classes="run-guard-control choice-select",
            )
            yield Static("每轮最多自动续跑次数", classes="run-guard-label")
            yield Input(
                str(cont.max_auto_followups),
                id="run-guard-followups",
                classes="run-guard-control",
            )
            yield Static(
                "保存后从下一次 Agent 回合生效；当前正在执行的回合不改变。",
                id="run-guard-status",
            )
        with Horizontal(id="run-guard-actions"):
            yield Static("", classes="pane-save-hint")
            yield Button("取消", id="run-guard-cancel")
            yield Button("保存", variant="primary", id="run-guard-save")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "run-guard-save":
            self.action_save()
        elif event.button.id == "run-guard-cancel":
            self.action_exit()

    def activate(self) -> None:
        self.query_one("#run-guard-enabled", Select).focus()

    def action_cancel(self) -> None:
        self.request_back()

    def action_exit(self) -> None:
        """“取消”按钮：直接关闭整个设置面板。"""
        self.request_exit()

    def action_save(self) -> None:
        try:
            old = self._configuration
            guard = ReasoningGuardConfig(
                enabled=self._read_bool("run-guard-guard-enabled"),
                window_chars=self._read_int("run-guard-window"),
                substr_len=self._read_int("run-guard-substr"),
                repeat_ratio=self._read_float("run-guard-ratio"),
                check_every=self._read_int("run-guard-check"),
                max_blocks=self._read_int("run-guard-blocks"),
                max_chars=self._read_int("run-guard-chars"),
                max_guard_retries=self._read_int("run-guard-retries"),
                auto_retry_errors=tuple(
                    item.strip()
                    for item in self.query_one("#run-guard-errors", Input).value.split(",")
                    if item.strip()
                ),
            )
            configuration = RunGuardConfig(
                enabled=self._read_bool("run-guard-enabled"),
                guard=guard,
                continuation=ContinueConfig(
                    enabled=self._read_bool("run-guard-continue-enabled"),
                    max_auto_followups=self._read_int("run-guard-followups"),
                ),
            )
            if self._config_path is not None:
                save_run_guard_config(configuration, self._config_path)
            if self._apply_configuration is not None:
                try:
                    self._apply_configuration(configuration)
                except Exception:
                    if self._config_path is not None:
                        save_run_guard_config(old, self._config_path)
                    raise
        except (RunGuardConfigError, OSError, ValueError) as exc:
            self.query_one("#run-guard-status", Static).update(f"保存失败：{exc}")
            return
        self.flash_save_hint()
        self.commit(configuration)

    def _read_int(self, widget_id: str) -> int:
        return int(self.query_one(f"#{widget_id}", Input).value.strip())

    def _read_float(self, widget_id: str) -> float:
        return float(self.query_one(f"#{widget_id}", Input).value.strip())

    def _read_bool(self, widget_id: str) -> bool:
        return bool(self.query_one(f"#{widget_id}", Select).value)


class RunGuardSettingsScreen(ModalScreen[Optional[RunGuardConfig]]):
    """整屏薄壳：内嵌 RunGuardSettingsPane，保存 dismiss 配置、Esc 取消。"""

    BINDINGS = [
        Binding("escape", "cancel", "取消"),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    CSS = terminal_css("""
    RunGuardSettingsScreen { align: center middle; background: $terminal-overlay; }
    #run-guard-dialog { width: 96; max-width: 96%; height: 43; max-height: 95%; padding: 1 2; border: round $terminal-border-strong; background: $terminal-surface; }
    #run-guard-title { height: 1; margin-bottom: 1; color: $terminal-white; text-style: bold; }
    """ + _PANE_CSS + terminal_select_css())

    def __init__(
        self,
        config_path: str | Path,
        *,
        configuration: RunGuardConfig | None = None,
        apply_configuration: Any | None = None,
    ) -> None:
        super().__init__()
        self._config_path = Path(config_path)
        self._apply_configuration = apply_configuration
        self._configuration = configuration
        self._pane: Optional[RunGuardSettingsPane] = None

    def compose(self) -> ComposeResult:
        with Container(id="run-guard-dialog"):
            yield Static("持续运转与自动续跑", id="run-guard-title")
            self._pane = RunGuardSettingsPane(
                self._config_path,
                configuration=self._configuration,
                apply_configuration=self._apply_configuration,
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


__all__ = ["RunGuardSettingsPane", "RunGuardSettingsScreen"]
