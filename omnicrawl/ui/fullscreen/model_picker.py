"""双列模型选择 Screen：渠道选择 + 当前渠道模型。

网络发现与切换在 worker 中执行；UI 更新只通过主线程回调完成。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Callable, Optional, Sequence

from rich.text import Text
from textual import events, work
from textual.app import ComposeResult
from textual.containers import Container, Horizontal, Vertical
from textual.screen import ModalScreen
from textual.widgets import Input, Static

from ...config.channels import ChannelConfig, load_channel_configuration
from ...config.llm import ActiveModelRef, save_active_model_ref
from ...config.model_catalog import (
    CatalogModel,
    ModelCatalogError,
    build_catalog,
    clear_discovery_cache,
    save_llm_model,
)
from ...agent import AgentError
from .theme import (
    ACCENT_GREEN,
    TEXT_MUTED,
    TEXT_PRIMARY,
    TEXT_SECONDARY,
    terminal_css,
)


@dataclass(frozen=True)
class _ChannelChoice:
    """模型切换页左列展示的安全渠道摘要。"""

    key: str
    name: str
    profile_id: str
    provider: str
    protocol: str
    model_id: str = ""
    source: str = "custom"
    enabled: bool = True


@dataclass(frozen=True)
class ModelPickerResult:
    """模型选择结果；取消时 dismiss(None)。"""

    model: str
    source: str
    key: str = ""
    profile: str = ""
    protocol: str = ""
    model_id: str = ""
    message: str = ""


class _ModelList(Static):
    """可聚焦的模型列表，避免初始键盘焦点被搜索框独占。"""

    can_focus = True


class ModelPickerScreen(ModalScreen[Optional[ModelPickerResult]]):
    """宽终端左右双列，窄终端上下分区。"""

    BINDINGS = [
        ("escape", "cancel", "取消"),
        ("r", "refresh", "刷新"),
        ("left", "column_left", "左列"),
        ("right", "column_right", "右列"),
        ("up", "move_up", "上"),
        ("down", "move_down", "下"),
        ("enter", "confirm", "切换"),
        ("slash", "focus_search", "搜索"),
    ]

    CSS = terminal_css("""
    ModelPickerScreen {
        align: center middle;
        background: $terminal-overlay;
    }
    #model-picker-dialog {
        width: 110;
        max-width: 96%;
        height: 28;
        max-height: 90%;
        padding: 1 2;
        border: solid $terminal-blue;
        background: $terminal-surface;
    }
    #model-picker-title {
        color: $terminal-green;
        text-style: bold;
        height: 1;
        margin-bottom: 1;
    }
    #model-picker-search {
        height: 3;
        border: none;
        background: $terminal-panel;
        color: $terminal-text;
        margin-bottom: 1;
    }
    #model-picker-search:focus {
        border-left: thick $terminal-green;
    }
    #model-picker-body {
        height: 1fr;
    }
    #model-picker-columns {
        height: 1fr;
    }
    .model-column {
        width: 1fr;
        height: 1fr;
        border: solid $terminal-border;
        padding: 0 1;
        background: $terminal-background;
    }
    .model-column.active-column {
        border: solid $terminal-green;
    }
    .model-column-title {
        color: $terminal-text-secondary;
        text-style: bold;
        height: 1;
        margin-bottom: 1;
    }
    .model-column-list {
        height: 1fr;
        color: $terminal-text;
    }
    #model-picker-diagnostics {
        height: auto;
        max-height: 3;
        color: $terminal-amber;
        margin-top: 1;
    }
    #model-picker-help {
        height: 1;
        color: $terminal-text-muted;
        margin-top: 1;
    }
    #model-picker-status {
        height: 1;
        color: $terminal-blue;
        margin-top: 0;
    }
    """)

    def __init__(
        self,
        agent: Any,
        *,
        refresh_on_open: bool = False,
        selection_only: bool = False,
        switch_model: Callable[[str], None] | None = None,
        persist_selection: Callable[[CatalogModel], str] | None = None,
    ) -> None:
        super().__init__()
        self._agent = agent
        self._refresh_on_open = refresh_on_open
        self._selection_only = selection_only
        self._switch_model = switch_model
        self._persist_selection = persist_selection or _default_persist_selection
        self._channels: list[_ChannelChoice] = []
        self._custom: list[CatalogModel] = []
        self._detected: list[CatalogModel] = []
        self._diagnostics: list[dict[str, str]] = []
        self._query = ""
        self._active_column = 0  # 0=渠道选择 1=当前渠道模型
        self._index_channels = 0
        self._index_models = 0
        self._loading = False
        self._switching = False
        self._status = "正在加载模型目录…"

    def compose(self) -> ComposeResult:
        with Container(id="model-picker-dialog"):
            yield Static(
                "选择视觉模型" if self._selection_only else "模型切换",
                id="model-picker-title",
            )
            yield Input(placeholder="搜索 key / 别名 / 模型 ID / provider / tag", id="model-picker-search")
            with Vertical(id="model-picker-body"):
                with Horizontal(id="model-picker-columns"):
                    with Vertical(classes="model-column active-column", id="column-channels"):
                        yield Static("渠道选择", classes="model-column-title")
                        yield _ModelList("", id="list-channels", classes="model-column-list")
                    with Vertical(classes="model-column", id="column-models"):
                        yield Static("模型", classes="model-column-title")
                        yield _ModelList("", id="list-models", classes="model-column-list")
                yield Static("", id="model-picker-diagnostics")
            yield Static("", id="model-picker-status")
            yield Static(
                "↑↓ 选择  ←→ 切换列  Tab 搜索  Enter 选择  / 搜索  R 刷新  Esc 取消"
                if self._selection_only
                else "↑↓ 选择  ←→ 切换列  Tab 搜索  Enter 切换  / 搜索  R 刷新  Esc 取消",
                id="model-picker-help",
            )

    def on_mount(self) -> None:
        # 模型切换是本界面的主操作，默认焦点必须落在列表上。搜索框仍可
        # 通过“/”或鼠标进入；搜索后按上下键会自动返回列表导航。
        # ModalScreen 会在 on_mount 之后执行默认自动聚焦，因此延后一帧才能
        # 稳定覆盖到模型列表，而不是被第一个可聚焦的搜索框重新抢回。
        self.call_after_refresh(self._focus_active_list)
        self._load_catalog(refresh=self._refresh_on_open)

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id != "model-picker-search":
            return
        self._query = event.value.strip().lower()
        self._index_channels = 0
        self._index_models = 0
        self._render_lists()

    def action_cancel(self) -> None:
        if self._switching:
            return
        self.dismiss(None)

    def action_refresh(self) -> None:
        if self._loading or self._switching:
            return
        self._load_catalog(refresh=True)

    def action_column_left(self) -> None:
        self._active_column = 0
        self._focus_active_list()
        self._render_lists()

    def action_column_right(self) -> None:
        self._active_column = 1
        self._focus_active_list()
        self._render_lists()

    def action_move_up(self) -> None:
        self._focus_active_list()
        if self._active_column == 0:
            channels = self._filtered_channels(self._channels)
            if channels:
                self._index_channels = (self._index_channels - 1) % len(channels)
        else:
            models = self._filtered_models()
            self._index_models = max(0, self._index_models - 1) if models else 0
        self._render_lists()

    def action_move_down(self) -> None:
        self._focus_active_list()
        if self._active_column == 0:
            channels = self._filtered_channels(self._channels)
            if channels:
                self._index_channels = (self._index_channels + 1) % len(channels)
        else:
            models = self._filtered_models()
            if models:
                self._index_models = min(len(models) - 1, self._index_models + 1)
        self._render_lists()

    def action_focus_search(self) -> None:
        if not self._loading and not self._switching:
            self.query_one("#model-picker-search", Input).focus()

    def action_confirm(self) -> None:
        if self._loading or self._switching:
            return
        selected = self._selected_item()
        if selected is None:
            self._status = "当前列没有可选模型。"
            self._render_status()
            return
        self._switch_to(selected)

    def on_key(self, event: events.Key) -> None:
        focused_id = getattr(self.focused, "id", "") if self.focused else ""
        if focused_id in {"list-channels", "list-models"} and event.key == "tab":
            # 两列模型列表都是可聚焦控件，Textual 默认 Tab 顺序会先从左列
            # 跳到右列，用户必须按两次才能进入搜索框。模型列表之间已经
            # 由左右键切换，因此 Tab 在任一列表中直接进入搜索框。
            event.prevent_default()
            event.stop()
            self.action_focus_search()
            return
        if focused_id != "model-picker-search":
            return
        if event.key in {"up", "down"}:
            # 真实终端中 Input 可能先消费方向键，因此在 Screen 事件层明确接管，
            # 同时把焦点移回列表，保证后续左右切列和 Enter 均稳定工作。
            event.prevent_default()
            event.stop()
            if event.key == "up":
                self.action_move_up()
            else:
                self.action_move_down()
        elif event.key == "enter":
            # Enter 在搜索框也执行切换。
            event.prevent_default()
            event.stop()
            self.action_confirm()

    def _focus_active_list(self) -> None:
        if not self.is_mounted:
            return
        list_id = "#list-channels" if self._active_column == 0 else "#list-models"
        self.query_one(list_id, _ModelList).focus()

    def _load_catalog(self, *, refresh: bool) -> None:
        self._loading = True
        self._status = "正在刷新模型目录…" if refresh else "正在加载模型目录…"
        self._render_status()
        self._fetch_catalog(refresh=refresh)

    @work(thread=True, exclusive=True, group="model-picker-catalog", exit_on_error=False)
    def _fetch_catalog(self, *, refresh: bool) -> None:
        try:
            if refresh:
                clear_discovery_cache()
            catalog = build_catalog(config=self._agent.config.llm, refresh=refresh)
            channel_error = ""
            try:
                channel_configuration = load_channel_configuration()
                channels = [
                    _channel_choice_from_config(item)
                    for item in channel_configuration.channels
                ]
            except Exception as exc:
                channels = []
                channel_error = str(exc)
            payload = {
                "channels": channels,
                "channel_error": channel_error,
                "custom": list(catalog.get("custom") or []),
                "detected": list(catalog.get("detected") or []),
                "diagnostics": list(catalog.get("diagnostics") or []),
                "error": "",
            }
        except Exception as exc:
            payload = {
                "channels": [],
                "channel_error": "",
                "custom": [],
                "detected": [],
                "diagnostics": [],
                "error": str(exc),
            }
        self.app.call_from_thread(self._apply_catalog, payload)

    def _apply_catalog(self, payload: dict[str, Any]) -> None:
        self._loading = False
        if payload.get("error"):
            self._status = f"目录加载失败：{payload['error']}"
            self._channels = []
            self._custom = []
            self._detected = []
            self._diagnostics = []
        else:
            self._custom = list(payload.get("custom") or [])
            self._detected = list(payload.get("detected") or [])
            configured_channels = []
            for item in list(payload.get("channels") or []):
                if isinstance(item, _ChannelChoice):
                    configured_channels.append(item)
                elif isinstance(item, ChannelConfig):
                    configured_channels.append(_channel_choice_from_config(item))

            self._channels = self._merge_channel_choices(
                configured_channels,
                self._custom,
                self._detected,
            )
            self._diagnostics = list(payload.get("diagnostics") or [])
            channel_error = str(payload.get("channel_error") or "").strip()
            if channel_error:
                self._diagnostics.append(
                    {"profile": "渠道", "message": f"渠道列表读取失败：{channel_error}"}
                )
            self._status = "正在整理渠道列表…"
            self._select_current_entries()
        self._index_channels = min(
            self._index_channels,
            max(0, len(self._filtered_channels(self._channels)) - 1),
        )
        self._index_models = min(
            self._index_models,
            max(0, len(self._filtered_models()) - 1),
        )
        self._render_lists()
        self._render_status()
        self._render_diagnostics()

    def _switch_to(self, item: CatalogModel) -> None:
        self._switching = True
        action = "选择" if self._selection_only else "切换"
        self._status = f"正在{action} {item.display_name or item.model_id}…"
        self._render_status()
        self._perform_switch(item)

    @work(thread=True, exclusive=True, group="model-picker-switch", exit_on_error=False)
    def _perform_switch(self, item: CatalogModel) -> None:
        try:
            token = _selection_token(item)
            message = ""

            if not self._selection_only:
                def persist() -> None:
                    nonlocal message
                    message = self._persist_selection(item)

                if self._switch_model is not None:
                    # 测试/嵌入调用可继续提供旧式回调。
                    # 先 persist 再切换，保证磁盘失败时内存模型不变。
                    persist()
                    self._switch_model(token)
                else:
                    self._agent.set_model(token, persist=persist)
            result = ModelPickerResult(
                model=token,
                source=item.source,
                key=item.key if item.source == "custom" else "",
                profile=item.profile_id,
                protocol=item.protocol,
                model_id=item.model_id,
                message=message,
            )
            self.app.call_from_thread(self.dismiss, result)
        except (AgentError, ModelCatalogError, Exception) as exc:
            self.app.call_from_thread(self._switch_failed, str(exc))

    def _switch_failed(self, message: str) -> None:
        self._switching = False
        self._status = f"切换失败：{message}"
        self._render_status()

    def _selected_item(self) -> CatalogModel | None:
        if self._active_column == 0:
            channel = self._selected_channel()
            if channel is None:
                return None
            candidates = [
                item
                for item in self._custom
                if item.key == channel.key
                or (
                    item.profile_id == channel.profile_id
                    and item.model_id == channel.model_id
                )
            ]
            if candidates:
                return candidates[0]
            candidates = [
                item
                for item in self._detected
                if item.profile_id == channel.profile_id
                and (not channel.model_id or item.model_id == channel.model_id)
            ]
            return candidates[0] if candidates else None

        models = self._filtered_models()
        if not models:
            return None
        index = max(0, min(self._index_models, len(models) - 1))
        return models[index]

    def _selected_channel(self) -> _ChannelChoice | None:
        channels = self._filtered_channels(self._channels)
        if not channels:
            return None
        index = max(0, min(self._index_channels, len(channels) - 1))
        return channels[index]

    def _models_for_channel(self) -> list[CatalogModel]:
        channel = self._selected_channel()
        if channel is None or not channel.profile_id:
            return list(self._detected)
        return [
            item for item in self._detected if item.profile_id == channel.profile_id
        ]

    def _filtered_models(self) -> list[CatalogModel]:
        return self._filtered(self._models_for_channel())

    def _filtered_channels(
        self, channels: Sequence[_ChannelChoice]
    ) -> list[_ChannelChoice]:
        query = self._query
        if not query:
            return [item for item in channels if item.enabled]
        result: list[_ChannelChoice] = []
        for item in channels:
            if not item.enabled:
                continue
            haystack = " ".join(
                [
                    item.key,
                    item.name,
                    item.profile_id,
                    item.provider,
                    item.protocol,
                    item.model_id,
                ]
            ).lower()
            if query in haystack:
                result.append(item)
        return result

    def _filtered(self, items: Sequence[CatalogModel]) -> list[CatalogModel]:
        query = self._query
        if not query:
            return list(items)
        result: list[CatalogModel] = []
        for item in items:
            haystack = " ".join(
                [
                    item.key,
                    item.display_name,
                    item.model_id,
                    item.provider,
                    item.protocol,
                    item.profile_id,
                    " ".join(item.aliases),
                    " ".join(item.tags),
                ]
            ).lower()
            if query in haystack:
                result.append(item)
        return result

    def _merge_channel_choices(
        self,
        configured: Sequence[_ChannelChoice],
        custom: Sequence[CatalogModel],
        detected: Sequence[CatalogModel],
    ) -> list[_ChannelChoice]:
        """优先使用渠道配置，同时补齐只有自动发现结果的 Profile。"""

        choices = list(configured)
        known_keys = {item.key for item in choices}
        blocked_profiles = {
            item.profile_id for item in choices if not item.enabled and item.profile_id
        }
        for item in custom:
            if item.key in known_keys:
                continue
            choices.append(_channel_choice_from_catalog(item))
            known_keys.add(item.key)
        known_profiles = {
            item.profile_id for item in choices if item.enabled and item.profile_id
        }
        for item in detected:
            if not item.profile_id or item.profile_id in known_profiles:
                continue
            if item.profile_id in blocked_profiles:
                continue
            choices.append(_channel_choice_from_catalog(item, source="detected"))
            known_profiles.add(item.profile_id)
        return choices

    def _select_current_entries(self) -> None:
        current = self._current_model()
        current_item = next(
            (
                item
                for item in [*self._custom, *self._detected]
                if _is_current(item, current)
            ),
            None,
        )
        llm = getattr(getattr(self._agent, "config", None), "llm", None)
        current_key = str(getattr(llm, "catalog_key", "") or "")
        channels = self._filtered_channels(self._channels)
        exact_keys = {
            value
            for value in (
                current_key,
                current_item.key if current_item is not None else "",
            )
            if value
        }
        exact_index = next(
            (index for index, channel in enumerate(channels) if channel.key in exact_keys),
            None,
        )
        if exact_index is not None:
            self._index_channels = exact_index
        else:
            current_profile = str(getattr(llm, "profile_id", "") or "")
            profile_id = current_item.profile_id if current_item is not None else current_profile
            current_model_id = current_item.model_id if current_item is not None else ""
            profile_index = next(
                (
                    index
                    for index, channel in enumerate(channels)
                    if channel.profile_id == profile_id
                    and (
                        not current_model_id
                        or not channel.model_id
                        or channel.model_id == current_model_id
                    )
                ),
                None,
            )
            if profile_index is not None:
                self._index_channels = profile_index

        models = self._filtered_models()
        for index, item in enumerate(models):
            if _is_current(item, current):
                self._index_models = index
                break

    def _render_lists(self) -> None:
        channels = self._filtered_channels(self._channels)
        models = self._filtered_models()
        if channels:
            self._index_channels = max(0, min(self._index_channels, len(channels) - 1))
        else:
            self._index_channels = 0
        if models:
            self._index_models = max(0, min(self._index_models, len(models) - 1))
        else:
            self._index_models = 0

        self.query_one("#list-channels", Static).update(
            self._render_channel_text(
                channels,
                selected=self._index_channels,
                current=self._current_model(),
            )
        )
        self.query_one("#list-models", Static).update(
            self._render_column_text(
                models,
                selected=self._index_models,
                current=self._current_model(),
            )
        )
        self.query_one("#column-channels").set_class(
            self._active_column == 0, "active-column"
        )
        self.query_one("#column-models").set_class(
            self._active_column == 1, "active-column"
        )
        if self._channels and not self._loading and not self._switching:
            channel = self._selected_channel()
            channel_name = channel.name if channel is not None else "未选择"
            self._status = (
                f"渠道 {len(channels)} · 当前：{channel_name} · 可用模型 {len(models)}"
            )
            self._render_status()

    def _render_channel_text(
        self,
        channels: Sequence[_ChannelChoice],
        *,
        selected: int,
        current: str,
    ) -> Text:
        if not channels:
            return Text("（空）", style=TEXT_MUTED)
        rendered = Text()
        window_size = 4
        selected = max(0, min(selected, len(channels) - 1))
        window_start = max(
            0,
            min(selected - window_size // 2, max(0, len(channels) - window_size)),
        )
        window_end = min(len(channels), window_start + window_size)
        if window_start > 0:
            rendered.append(f"... 前面 {window_start} 个\n", style=TEXT_MUTED)
        for index, channel in enumerate(
            channels[window_start:window_end],
            start=window_start,
        ):
            is_selected = index == selected
            is_current = _is_current_channel(channel, current)
            marker = "●" if is_current else ("›" if is_selected else " ")
            style = (
                f"{ACCENT_GREEN} bold"
                if is_selected
                else TEXT_PRIMARY
                if is_current
                else TEXT_SECONDARY
            )
            title = channel.name or channel.key
            rendered.append(f"{marker} {title}\n", style=style)
        remaining = len(channels) - window_end
        if remaining > 0:
            rendered.append(f"... 后面 {remaining} 个\n", style=TEXT_MUTED)
        return rendered

    def _render_column_text(
        self,
        items: Sequence[CatalogModel],
        *,
        selected: int,
        current: str,
    ) -> Text:
        if not items:
            return Text("（空）", style=TEXT_MUTED)
        rendered = Text()
        # 列表区域高度固定，使用跟随选中项的窗口而不是永远截取前 40 项。
        # 这样方向键可以访问并看见发现列表中的每个模型。
        window_size = 4
        selected = max(0, min(selected, len(items) - 1))
        window_start = max(
            0,
            min(selected - window_size // 2, max(0, len(items) - window_size)),
        )
        window_end = min(len(items), window_start + window_size)
        if window_start > 0:
            rendered.append(f"... 前面 {window_start} 项\n", style=TEXT_MUTED)
        for index, item in enumerate(
            items[window_start:window_end],
            start=window_start,
        ):
            is_selected = index == selected
            is_current = _is_current(item, current)
            marker = "●" if is_current else ("›" if is_selected else " ")
            style = (
                f"{ACCENT_GREEN} bold"
                if is_selected
                else TEXT_PRIMARY
                if is_current
                else TEXT_SECONDARY
            )
            title = item.display_name or item.model_id
            rendered.append(f"{marker} {title}\n", style=style)
        remaining = len(items) - window_end
        if remaining > 0:
            rendered.append(f"... 后面 {remaining} 项\n", style=TEXT_MUTED)
        return rendered

    def _render_status(self) -> None:
        self.query_one("#model-picker-status", Static).update(self._status)

    def _render_diagnostics(self) -> None:
        if not self._diagnostics:
            self.query_one("#model-picker-diagnostics", Static).update("")
            return
        lines = []
        for item in self._diagnostics[:3]:
            profile = item.get("profile") or "?"
            message = item.get("message") or item.get("status") or "发现失败"
            lines.append(f"! {profile}: {message}")
        self.query_one("#model-picker-diagnostics", Static).update("\n".join(lines))

    def _current_model(self) -> str:
        return str(getattr(self._agent, "current_model", "") or "")


def _channel_choice_from_config(channel: ChannelConfig) -> _ChannelChoice:
    return _ChannelChoice(
        key=channel.key,
        name=channel.name,
        profile_id=channel.profile_id,
        provider=channel.provider,
        protocol=channel.protocol,
        model_id=channel.model_id,
        source="custom",
        enabled=channel.enabled,
    )


def _channel_choice_from_catalog(
    item: CatalogModel,
    *,
    source: str | None = None,
) -> _ChannelChoice:
    return _ChannelChoice(
        key=item.key or f"{item.profile_id}/{item.model_id}",
        name=item.display_name or item.profile_id or item.model_id,
        profile_id=item.profile_id,
        provider=item.provider,
        protocol=item.protocol,
        model_id=item.model_id,
        source=source or item.source,
    )


def _is_current_channel(channel: _ChannelChoice, current: str) -> bool:
    current = (current or "").strip()
    return bool(
        current
        and (
            current == channel.key
            or current == channel.model_id
            or current == f"{channel.profile_id}/{channel.model_id}"
        )
    )


def _selection_token(item: CatalogModel) -> str:
    if item.source == "custom" and item.key:
        return item.key
    if item.profile_id:
        return f"{item.profile_id}/{item.model_id}"
    return item.model_id


def _is_current(item: CatalogModel, current: str) -> bool:
    current = (current or "").strip()
    if not current:
        return False
    if item.key and current == item.key:
        return True
    if current == item.model_id:
        return True
    if item.profile_id and current == f"{item.profile_id}/{item.model_id}":
        return True
    return current in item.aliases


def _default_persist_selection(item: CatalogModel) -> str:
    if item.source == "custom" and item.key:
        path = save_active_model_ref(
            ActiveModelRef(source="custom", key=item.key, model_id=item.model_id)
        )
        return f"已切换为渠道 {item.key}，并写入 {path}"
    if item.profile_id:
        path = save_active_model_ref(
            ActiveModelRef(
                source="detected",
                profile=item.profile_id,
                model_id=item.model_id,
                protocol=item.protocol,
            )
        )
        return f"已切换为 {item.profile_id}/{item.model_id}，并写入 {path}"
    path = save_llm_model(item.model_id)
    return f"已切换为 {item.model_id}，并写入 {path}"


__all__ = ["ModelPickerResult", "ModelPickerScreen"]
