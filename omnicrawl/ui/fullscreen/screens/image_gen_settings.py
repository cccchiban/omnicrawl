"""图像生成（OpenAI 兼容 Image API）的设置界面。"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Input, Select, Static

from ....config.features.image_gen import (
    ImageGenConfigError,
    ImageGenConfiguration,
    load_image_gen_configuration,
    save_image_gen_configuration,
)
from ..terminal.theme import terminal_css, terminal_select_css

_SIZES = (
    "auto",
    "1024x1024",
    "1536x1024",
    "1024x1536",
    "2048x2048",
    "2048x1152",
    "3840x2160",
    "2160x3840",
)
_QUALITIES = ("auto", "low", "medium", "high")
_FORMATS = ("png", "jpeg", "webp")
_COUNTS = (1, 2, 3, 4)
_TIMEOUTS = (30, 60, 120, 180, 300, 600)


@dataclass(frozen=True)
class ImageGenSettingsResult:
    """图像生成设置保存结果。"""

    configuration: ImageGenConfiguration
    config_path: Path


class ImageGenSettingsScreen(ModalScreen[Optional[ImageGenSettingsResult]]):
    """编辑图像生成接口地址、API Key、模型与默认参数。"""

    BINDINGS = [
        Binding("escape", "cancel", "取消"),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    CSS = terminal_css("""
    ImageGenSettingsScreen {
        align: center middle;
        background: $terminal-overlay;
    }
    #image-gen-dialog {
        width: 92;
        max-width: 96%;
        height: 42;
        max-height: 95%;
        padding: 1 2;
        border: round $terminal-border-strong;
        background: $terminal-surface;
    }
    #image-gen-title {
        height: 1;
        margin-bottom: 1;
        color: $terminal-white;
        text-style: bold;
    }
    #image-gen-form {
        height: 1fr;
    }
    .image-gen-field-label {
        height: 1;
        color: $terminal-text-muted;
    }
    .image-gen-control {
        height: 3;
        margin-bottom: 1;
    }
    #image-gen-status {
        height: 2;
        color: $terminal-white;
    }
    #image-gen-actions {
        height: 3;
        align-horizontal: right;
    }
    """ + terminal_select_css())

    def __init__(
        self,
        config_path: str | Path,
        *,
        apply_configuration: Callable[[ImageGenConfiguration], None] | None = None,
    ) -> None:
        super().__init__()
        self._config_path = Path(config_path)
        self._apply_configuration = apply_configuration
        configuration = load_image_gen_configuration(self._config_path)
        self._previous_configuration = configuration
        self._enabled = configuration.enabled

    def compose(self) -> ComposeResult:
        c = self._previous_configuration
        with Container(id="image-gen-dialog"):
            yield Static("图像生成配置（OpenAI 兼容 Image API）", id="image-gen-title")
            with VerticalScroll(id="image-gen-form"):
                yield Static("启用图像生成工具", classes="image-gen-field-label")
                yield Select(
                    [("停用", False), ("启用", True)],
                    value=self._enabled,
                    allow_blank=False,
                    id="image-gen-enabled",
                    classes="image-gen-control choice-select",
                )
                yield Static("接口地址 base_url（官方或 OpenAI 兼容中转站）", classes="image-gen-field-label")
                yield Input(
                    c.base_url,
                    placeholder="https://api.openai.com/v1",
                    id="image-gen-base-url",
                    classes="image-gen-control",
                )
                yield Static("API Key（留空则读取下方环境变量）", classes="image-gen-field-label")
                yield Input(
                    c.api_key,
                    placeholder="sk-...（仅写入本地 config.toml）",
                    password=True,
                    id="image-gen-api-key",
                    classes="image-gen-control",
                )
                yield Static("API Key 环境变量名（api_key 为空时回退）", classes="image-gen-field-label")
                yield Input(
                    c.api_key_env,
                    placeholder="OPENAI_API_KEY",
                    id="image-gen-api-key-env",
                    classes="image-gen-control",
                )
                yield Static("模型 model", classes="image-gen-field-label")
                yield Input(
                    c.model,
                    placeholder="gpt-image-2",
                    id="image-gen-model",
                    classes="image-gen-control",
                )
                yield Static("默认尺寸 size（auto 或 宽x高）", classes="image-gen-field-label")
                yield Select(
                    [(item, item) for item in _SIZES],
                    value=c.size if c.size in _SIZES else "auto",
                    allow_blank=False,
                    id="image-gen-size",
                    classes="image-gen-control choice-select",
                )
                yield Static("默认质量 quality", classes="image-gen-field-label")
                yield Select(
                    [(item, item) for item in _QUALITIES],
                    value=c.quality,
                    allow_blank=False,
                    id="image-gen-quality",
                    classes="image-gen-control choice-select",
                )
                yield Static("默认输出格式 output_format", classes="image-gen-field-label")
                yield Select(
                    [(item, item) for item in _FORMATS],
                    value=c.output_format,
                    allow_blank=False,
                    id="image-gen-format",
                    classes="image-gen-control choice-select",
                )
                yield Static("每次默认生成张数 n", classes="image-gen-field-label")
                yield Select(
                    [(str(item), item) for item in _COUNTS],
                    value=c.n if c.n in _COUNTS else 1,
                    allow_blank=False,
                    id="image-gen-count",
                    classes="image-gen-control choice-select",
                )
                yield Static("请求超时 timeout_seconds", classes="image-gen-field-label")
                yield Select(
                    [(str(item), item) for item in _TIMEOUTS],
                    value=c.timeout_seconds if c.timeout_seconds in _TIMEOUTS else 120,
                    allow_blank=False,
                    id="image-gen-timeout",
                    classes="image-gen-control choice-select",
                )
                yield Static(" ", id="image-gen-status")
            with Horizontal(id="image-gen-actions"):
                yield Button("取消", id="image-gen-cancel")
                yield Button("保存", variant="primary", id="image-gen-save")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "image-gen-save":
            self.action_save()
        elif event.button.id == "image-gen-cancel":
            self.action_cancel()

    def action_cancel(self) -> None:
        self.dismiss(None)

    def action_save(self) -> None:
        try:
            configuration = ImageGenConfiguration(
                enabled=bool(self.query_one("#image-gen-enabled", Select).value),
                base_url=self._read_input("image-gen-base-url", fallback="https://api.openai.com/v1"),
                api_key=self._read_input("image-gen-api-key", fallback=""),
                api_key_env=self._read_input("image-gen-api-key-env", fallback="OPENAI_API_KEY"),
                model=self._read_input("image-gen-model", fallback="gpt-image-2"),
                size=str(self.query_one("#image-gen-size", Select).value),
                quality=str(self.query_one("#image-gen-quality", Select).value),
                output_format=str(self.query_one("#image-gen-format", Select).value),
                n=int(self.query_one("#image-gen-count", Select).value),
                timeout_seconds=int(self.query_one("#image-gen-timeout", Select).value),
            )
            path = save_image_gen_configuration(configuration, self._config_path)
            if self._apply_configuration is not None:
                try:
                    self._apply_configuration(configuration)
                except Exception:
                    save_image_gen_configuration(self._previous_configuration, self._config_path)
                    raise
        except (ImageGenConfigError, OSError, ValueError) as exc:
            self._set_status(f"保存失败：{exc}")
            return
        self.dismiss(ImageGenSettingsResult(configuration, path))

    def _read_input(self, widget_id: str, *, fallback: str) -> str:
        value = self.query_one(f"#{widget_id}", Input).value.strip()
        return value or fallback

    def _set_status(self, status: str) -> None:
        self.query_one("#image-gen-status", Static).update(status)


__all__ = ["ImageGenSettingsResult", "ImageGenSettingsScreen"]
