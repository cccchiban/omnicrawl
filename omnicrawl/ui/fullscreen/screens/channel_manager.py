"""首次启动与运行设置共用的模型渠道管理界面。"""

from __future__ import annotations

from dataclasses import dataclass, replace
from pathlib import Path
from typing import Callable, Optional

from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Input, Label, Select, Static

from ....config.models.channels import (
    ChannelConfig,
    ChannelConfigError,
    ChannelConfiguration,
    default_channel,
    has_usable_channel,
    load_channel_configuration,
    missing_enabled_credentials,
    protocol_label,
    protocols_for_provider,
    provider_label,
    provider_options,
    save_channel_configuration,
    unique_channel_key,
)
from ..terminal.theme import terminal_css, terminal_select_css


@dataclass(frozen=True)
class ChannelManagerResult:
    """渠道管理界面成功保存后的结果。"""

    configuration: ChannelConfiguration
    config_path: Path
    models_path: Path


class ChannelEditorScreen(ModalScreen[Optional[ChannelConfig]]):
    """编辑单个渠道；敏感字段使用密码输入框，不写日志。"""

    BINDINGS = [
        ("escape", "cancel", "取消"),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    CSS = terminal_css("""
    ChannelEditorScreen {
        align: center middle;
        background: $terminal-overlay;
    }
    #channel-editor-dialog {
        width: 78;
        max-width: 95%;
        height: 38;
        max-height: 94%;
        padding: 1 2;
        border: round $terminal-white;
        background: $terminal-surface;
    }
    #channel-editor-title {
        height: 1;
        margin-bottom: 1;
        color: $terminal-white;
        text-style: bold;
    }
    #channel-editor-form {
        height: 1fr;
    }
    .channel-editor-label {
        height: 1;
        color: $terminal-text-secondary;
    }
    .channel-editor-control {
        height: 3;
        margin-bottom: 1;
    }
    #channel-editor-actions {
        height: 3;
        align-horizontal: right;
    }
    #channel-editor-status {
        height: 2;
        color: $terminal-white;
    }
    """ + terminal_select_css())

    def __init__(
        self,
        channel: ChannelConfig | None,
        *,
        existing_keys: set[str],
    ) -> None:
        super().__init__()
        self._original = channel
        self._existing_keys = set(existing_keys)
        self._draft = channel or default_channel("openai", key="new-channel")
        self._provider = self._draft.provider

    def compose(self) -> ComposeResult:
        with Container(id="channel-editor-dialog"):
            yield Static(
                "编辑模型渠道" if self._original is not None else "添加模型渠道",
                id="channel-editor-title",
            )
            with VerticalScroll(id="channel-editor-form"):
                yield Label("请求方式", classes="channel-editor-label")
                yield Select(
                    [(provider_label(item), item) for item in provider_options()],
                    value=self._draft.provider,
                    allow_blank=False,
                    id="channel-editor-provider",
                    classes="channel-editor-control choice-select",
                )
                yield Label("请求协议", classes="channel-editor-label")
                yield Select(
                    [
                        (protocol_label(item), item)
                        for item in protocols_for_provider(self._draft.provider)
                    ],
                    value=self._draft.protocol,
                    allow_blank=False,
                    id="channel-editor-protocol",
                    classes="channel-editor-control choice-select",
                )
                yield Label("渠道名称", classes="channel-editor-label")
                yield Input(
                    self._draft.name,
                    placeholder="例如：OpenAI 官方 / 公司代理",
                    id="channel-editor-name",
                    classes="channel-editor-control",
                )
                yield Label("Base URL", classes="channel-editor-label")
                yield Input(
                    self._draft.base_url,
                    placeholder="https://api.example.com/v1",
                    id="channel-editor-url",
                    classes="channel-editor-control",
                )
                yield Label("User-Agent（可选）", classes="channel-editor-label")
                yield Input(
                    self._draft.user_agent,
                    placeholder="例如：OmniCrawl/1.0 或公司网关要求的 UA",
                    id="channel-editor-user-agent",
                    classes="channel-editor-control",
                )
                yield Label("API Key", classes="channel-editor-label")
                yield Input(
                    self._draft.api_key,
                    placeholder="输入内容不会明文显示",
                    password=True,
                    id="channel-editor-key",
                    classes="channel-editor-control",
                )
                yield Label("模型 ID", classes="channel-editor-label")
                yield Input(
                    self._draft.model_id,
                    placeholder="例如：gpt-5.2",
                    id="channel-editor-model",
                    classes="channel-editor-control",
                )
                yield Static("", id="channel-editor-status")
            with Horizontal(id="channel-editor-actions"):
                yield Button("取消", id="channel-editor-cancel")
                yield Button("保存", variant="primary", id="channel-editor-save")

    def on_select_changed(self, event: Select.Changed) -> None:
        if event.select.id != "channel-editor-provider":
            return
        provider = str(event.value)
        if provider == self._provider:
            return
        previous = default_channel(self._provider)
        selected = default_channel(provider)
        self._provider = provider
        protocol_select = self.query_one("#channel-editor-protocol", Select)
        protocols = protocols_for_provider(provider)
        protocol_select.set_options([(protocol_label(item), item) for item in protocols])
        protocol_select.value = protocols[0]
        url_input = self.query_one("#channel-editor-url", Input)
        model_input = self.query_one("#channel-editor-model", Input)
        if not url_input.value.strip() or url_input.value.strip() == previous.base_url:
            url_input.value = selected.base_url
        if not model_input.value.strip() or model_input.value.strip() == previous.model_id:
            model_input.value = selected.model_id

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "channel-editor-save":
            self.action_save()
        elif event.button.id == "channel-editor-cancel":
            self.action_cancel()

    def action_cancel(self) -> None:
        self.dismiss(None)

    def action_save(self) -> None:
        provider = str(self.query_one("#channel-editor-provider", Select).value)
        protocol = str(self.query_one("#channel-editor-protocol", Select).value)
        name = self.query_one("#channel-editor-name", Input).value.strip()
        base_url = self.query_one("#channel-editor-url", Input).value.strip()
        user_agent = self.query_one("#channel-editor-user-agent", Input).value.strip()
        api_key = self.query_one("#channel-editor-key", Input).value.strip()
        model_id = self.query_one("#channel-editor-model", Input).value.strip()
        if not name or not base_url or not api_key or not model_id:
            self.query_one("#channel-editor-status", Static).update(
                "渠道名称、Base URL、API Key 和模型 ID 均不能为空。"
            )
            return

        if self._original is None:
            key = unique_channel_key(name, self._existing_keys)
            profile_id = key
            enabled = True
        else:
            key = self._original.key
            profile_id = self._original.profile_id
            enabled = self._original.enabled
        self.dismiss(
            ChannelConfig(
                key=key,
                name=name,
                profile_id=profile_id,
                provider=provider,
                protocol=protocol,
                base_url=base_url,
                api_key=api_key,
                api_key_env={
                    "openai": "OPENAI_API_KEY",
                    "anthropic": "ANTHROPIC_API_KEY",
                    "gemini": "GEMINI_API_KEY",
                }[provider],
                user_agent=user_agent,
                model_id=model_id,
                enabled=enabled,
            )
        )


class ChannelManagerScreen(ModalScreen[Optional[ChannelManagerResult]]):
    """方向键与勾选操作的渠道列表。"""

    BINDINGS = [
        ("escape", "cancel", "返回"),
        Binding("up", "move_up", "上一项", priority=True),
        Binding("down", "move_down", "下一项", priority=True),
        Binding("space", "toggle", "启用/禁用", priority=True),
        ("a", "add", "添加"),
        ("enter", "edit", "编辑"),
        ("d", "delete", "删除"),
        ("f", "make_default", "设为默认"),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    CSS = terminal_css("""
    ChannelManagerScreen {
        align: center middle;
        background: $terminal-overlay;
    }
    #channel-manager-dialog {
        width: 100;
        max-width: 96%;
        height: 32;
        max-height: 92%;
        padding: 1 2;
        border: round $terminal-border-strong;
        background: $terminal-surface;
    }
    #channel-manager-title {
        height: 1;
        margin-bottom: 1;
        color: $terminal-white;
        text-style: bold;
    }
    #channel-manager-list {
        height: 1fr;
        border: round $terminal-border;
        background: $terminal-background;
        padding: 0 1;
    }
    .channel-row {
        height: 2;
        padding: 0 1;
        color: $terminal-text-secondary;
    }
    .channel-row.selected {
        color: $terminal-amber;
        text-style: bold;
    }
    #channel-manager-status {
        height: 2;
        margin-top: 1;
        color: $terminal-white;
    }
    #channel-manager-help {
        height: 2;
        color: $terminal-white;
    }
    """)

    def __init__(
        self,
        config_path: Path,
        models_path: Path,
        *,
        required: bool = False,
        apply_configuration: Callable[[ChannelConfiguration], None] | None = None,
    ) -> None:
        super().__init__()
        self._config_path = Path(config_path)
        self._models_path = Path(models_path)
        self._required = required
        self._apply_configuration = apply_configuration
        loaded = load_channel_configuration(self._config_path, self._models_path)
        self._channels = list(loaded.channels)
        self._default_key = loaded.default_key
        if self._required:
            existing_providers = {item.provider for item in self._channels}
            existing_keys = {item.key for item in self._channels}
            for provider in provider_options():
                if provider in existing_providers:
                    continue
                draft = default_channel(provider)
                if draft.key in existing_keys:
                    unique_key = unique_channel_key(draft.name, existing_keys)
                    draft = replace(draft, key=unique_key, profile_id=unique_key)
                self._channels.append(replace(draft, enabled=False))
                existing_keys.add(draft.key)
        self._selected = 0
        self._status = "空格勾选启用渠道；编辑后按 Ctrl+S 保存全部配置。"
        self._pending_delete_key = ""

    def compose(self) -> ComposeResult:
        with Container(id="channel-manager-dialog"):
            yield Static(
                "首次启动：配置模型渠道" if self._required else "模型渠道管理",
                id="channel-manager-title",
            )
            with VerticalScroll(id="channel-manager-list"):
                if self._channels:
                    for index, channel in enumerate(self._channels):
                        yield Static(
                            self._row_text(channel, index),
                            classes="channel-row selected" if index == 0 else "channel-row",
                        )
                else:
                    yield Static(
                        "尚无渠道，按 A 添加第一个渠道。",
                        id="channel-empty-row",
                        classes="channel-row selected",
                    )
            yield Static(self._status, id="channel-manager-status")
            yield Static(
                "↑↓ 选择  空格勾选  Enter 编辑  A 添加  D 删除  F 默认  Ctrl+S 保存  Esc 返回",
                id="channel-manager-help",
            )

    def action_cancel(self) -> None:
        if self._required:
            self._set_status("首次启动必须保存至少一个可用渠道；按 Ctrl+S 完成配置。")
            return
        self.dismiss(None)

    def action_move_up(self) -> None:
        if self._channels:
            self._selected = (self._selected - 1) % len(self._channels)
            self._pending_delete_key = ""
            self._render_rows()

    def action_move_down(self) -> None:
        if self._channels:
            self._selected = (self._selected + 1) % len(self._channels)
            self._pending_delete_key = ""
            self._render_rows()

    def action_toggle(self) -> None:
        if not self._channels:
            return
        current = self._channels[self._selected]
        if current.enabled and sum(item.enabled for item in self._channels) == 1:
            self._set_status("至少需要保留一个启用渠道。")
            return
        self._channels[self._selected] = replace(current, enabled=not current.enabled)
        if not self._channels[self._selected].enabled and current.key == self._default_key:
            self._default_key = next(item.key for item in self._channels if item.enabled)
        self._pending_delete_key = ""
        self._render_rows()

    def action_add(self) -> None:
        self.app.push_screen(
            ChannelEditorScreen(None, existing_keys={item.key for item in self._channels}),
            self._receive_added,
        )

    def action_edit(self) -> None:
        if not self._channels:
            self.action_add()
            return
        current = self._channels[self._selected]
        self.app.push_screen(
            ChannelEditorScreen(current, existing_keys={item.key for item in self._channels}),
            self._receive_edited,
        )

    def action_delete(self) -> None:
        if not self._channels:
            return
        if len(self._channels) == 1:
            self._set_status("至少需要保留一个渠道。")
            return
        current = self._channels[self._selected]
        if current.enabled and sum(item.enabled for item in self._channels) == 1:
            self._set_status("至少需要保留一个启用渠道，请先启用其他渠道。")
            return
        if self._pending_delete_key != current.key:
            self._pending_delete_key = current.key
            self._set_status(f"再次按 D 确认删除渠道“{current.name}”。")
            return
        removed = self._channels.pop(self._selected)
        self._selected = min(self._selected, len(self._channels) - 1)
        if removed.key == self._default_key:
            self._default_key = next(
                (item.key for item in self._channels if item.enabled),
                self._channels[0].key,
            )
        self._pending_delete_key = ""
        self._set_status(f"已从待保存列表删除“{removed.name}”。")
        self._rebuild_rows()

    def action_make_default(self) -> None:
        if not self._channels:
            return
        current = self._channels[self._selected]
        if not current.enabled:
            self._set_status("请先勾选启用该渠道，再设为默认。")
            return
        self._default_key = current.key
        self._pending_delete_key = ""
        self._set_status(f"默认渠道已设为“{current.name}”，按 Ctrl+S 写入配置。")
        self._render_rows()

    def action_save(self) -> None:
        configuration = ChannelConfiguration(tuple(self._channels), self._default_key)
        missing_credentials = missing_enabled_credentials(configuration)
        if missing_credentials:
            self._set_status(
                "请为已勾选渠道配置 API Key：" + "、".join(missing_credentials)
            )
            return
        if self._required and not has_usable_channel(configuration):
            self._set_status("默认渠道必须配置 API Key，或对应环境变量必须已有值。")
            return
        try:
            config_path, models_path = save_channel_configuration(
                configuration,
                self._config_path,
                self._models_path,
            )
            saved = load_channel_configuration(config_path, models_path)
            if self._apply_configuration is not None:
                self._apply_configuration(saved)
        except (ChannelConfigError, OSError, RuntimeError) as exc:
            self._set_status(f"渠道配置保存失败：{exc}")
            return
        self.dismiss(ChannelManagerResult(saved, config_path, models_path))

    def _receive_added(self, channel: ChannelConfig | None) -> None:
        if channel is None:
            return
        self._channels.append(channel)
        if not self._default_key:
            self._default_key = channel.key
        self._selected = len(self._channels) - 1
        self._set_status(f"已添加“{channel.name}”，按 Ctrl+S 写入配置。")
        self._rebuild_rows()

    def _receive_edited(self, channel: ChannelConfig | None) -> None:
        if channel is None:
            return
        self._channels[self._selected] = channel
        self._set_status(f"已更新“{channel.name}”，按 Ctrl+S 写入配置。")
        self._render_rows()

    def _row_text(self, channel: ChannelConfig, index: int) -> str:
        cursor = "›" if index == self._selected else " "
        checked = "x" if channel.enabled else " "
        default = " [默认]" if channel.key == self._default_key else ""
        return (
            f"{cursor} [{checked}] {channel.name}{default}\n"
            f"    {channel.provider_label} / {channel.protocol_label}  {channel.model_id}  {channel.base_url}"
        )

    def _render_rows(self) -> None:
        if not self.is_mounted:
            return
        rows = list(self.query("#channel-manager-list .channel-row"))
        for index, (channel, row) in enumerate(zip(self._channels, rows)):
            row.update(self._row_text(channel, index))
            row.set_class(index == self._selected, "selected")
        self.query_one("#channel-manager-status", Static).update(self._status)

    def _rebuild_rows(self) -> None:
        if not self.is_mounted:
            return
        container = self.query_one("#channel-manager-list", VerticalScroll)
        container.remove_children()
        for index, channel in enumerate(self._channels):
            container.mount(
                Static(
                    self._row_text(channel, index),
                    classes="channel-row selected" if index == self._selected else "channel-row",
                )
            )

    def _set_status(self, message: str) -> None:
        self._status = message
        if self.is_mounted:
            self.query_one("#channel-manager-status", Static).update(message)

from .channel_setup import ChannelSetupApp, run_channel_setup


__all__ = [
    "ChannelEditorScreen",
    "ChannelManagerResult",
    "ChannelManagerScreen",
    "ChannelSetupApp",
    "run_channel_setup",
]
