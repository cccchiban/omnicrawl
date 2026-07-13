"""双列模型选择 Screen：自定义 models.yaml + 自动发现。

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

from ...config.llm import ActiveModelRef
from ...config.model_catalog import (
    CatalogModel,
    ModelCatalogError,
    build_catalog,
    clear_discovery_cache,
    save_llm_model,
)
from ...config.llm import save_active_model_ref
from ...agent import AgentError


@dataclass(frozen=True)
class ModelPickerResult:
    """模型选择结果；取消时 dismiss(None)。"""

    model: str
    source: str
    key: str = ""
    profile: str = ""
    protocol: str = ""
    message: str = ""


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
    ]

    CSS = """
    ModelPickerScreen {
        align: center middle;
        background: rgba(5, 8, 10, 0.92);
    }
    #model-picker-dialog {
        width: 110;
        max-width: 96%;
        height: 28;
        max-height: 90%;
        padding: 1 2;
        border: solid #39a7ff;
        background: #0b1014;
    }
    #model-picker-title {
        color: #00e5c3;
        text-style: bold;
        height: 1;
        margin-bottom: 1;
    }
    #model-picker-search {
        height: 3;
        border: none;
        background: #0a0e12;
        color: #d9e4e8;
        margin-bottom: 1;
    }
    #model-picker-search:focus {
        border-left: thick #00e5c3;
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
        border: solid #16232a;
        padding: 0 1;
        background: #0a0e12;
    }
    .model-column.active-column {
        border: solid #00e5c3;
    }
    .model-column-title {
        color: #8fa4ad;
        text-style: bold;
        height: 1;
        margin-bottom: 1;
    }
    .model-column-list {
        height: 1fr;
        color: #d9e4e8;
    }
    #model-picker-diagnostics {
        height: auto;
        max-height: 3;
        color: #f4b860;
        margin-top: 1;
    }
    #model-picker-help {
        height: 1;
        color: #59676d;
        margin-top: 1;
    }
    #model-picker-status {
        height: 1;
        color: #39a7ff;
        margin-top: 0;
    }
    """

    def __init__(
        self,
        agent: Any,
        *,
        refresh_on_open: bool = False,
        switch_model: Callable[[str], None] | None = None,
        persist_selection: Callable[[CatalogModel], str] | None = None,
    ) -> None:
        super().__init__()
        self._agent = agent
        self._refresh_on_open = refresh_on_open
        self._switch_model = switch_model
        self._persist_selection = persist_selection or _default_persist_selection
        self._custom: list[CatalogModel] = []
        self._detected: list[CatalogModel] = []
        self._diagnostics: list[dict[str, str]] = []
        self._query = ""
        self._active_column = 0  # 0=custom 1=detected
        self._index_custom = 0
        self._index_detected = 0
        self._loading = False
        self._switching = False
        self._status = "正在加载模型目录…"

    def compose(self) -> ComposeResult:
        with Container(id="model-picker-dialog"):
            yield Static("模型切换", id="model-picker-title")
            yield Input(placeholder="搜索 key / 别名 / 模型 ID / provider / tag", id="model-picker-search")
            with Vertical(id="model-picker-body"):
                with Horizontal(id="model-picker-columns"):
                    with Vertical(classes="model-column active-column", id="column-custom"):
                        yield Static("自定义模型", classes="model-column-title")
                        yield Static("", id="list-custom", classes="model-column-list")
                    with Vertical(classes="model-column", id="column-detected"):
                        yield Static("自动检测", classes="model-column-title")
                        yield Static("", id="list-detected", classes="model-column-list")
                yield Static("", id="model-picker-diagnostics")
            yield Static("", id="model-picker-status")
            yield Static(
                "↑↓ 选择  ←→ 切换列  Enter 切换  R 刷新  Esc 取消",
                id="model-picker-help",
            )

    def on_mount(self) -> None:
        self.query_one("#model-picker-search", Input).focus()
        self._load_catalog(refresh=self._refresh_on_open)

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id != "model-picker-search":
            return
        self._query = event.value.strip().lower()
        self._index_custom = 0
        self._index_detected = 0
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
        self._render_lists()

    def action_column_right(self) -> None:
        self._active_column = 1
        self._render_lists()

    def action_move_up(self) -> None:
        if self._active_column == 0:
            self._index_custom = max(0, self._index_custom - 1)
        else:
            self._index_detected = max(0, self._index_detected - 1)
        self._render_lists()

    def action_move_down(self) -> None:
        custom = self._filtered(self._custom)
        detected = self._filtered(self._detected)
        if self._active_column == 0:
            if custom:
                self._index_custom = min(len(custom) - 1, self._index_custom + 1)
        else:
            if detected:
                self._index_detected = min(len(detected) - 1, self._index_detected + 1)
        self._render_lists()

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
        # 搜索框聚焦时，R/方向键仍交给 bindings；字符输入留给 Input。
        if event.key in {"enter"} and self.focused and getattr(self.focused, "id", "") == "model-picker-search":
            # Enter 在搜索框也执行切换。
            event.stop()
            self.action_confirm()

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
            payload = {
                "custom": list(catalog.get("custom") or []),
                "detected": list(catalog.get("detected") or []),
                "diagnostics": list(catalog.get("diagnostics") or []),
                "error": "",
            }
        except Exception as exc:
            payload = {
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
            self._custom = []
            self._detected = []
            self._diagnostics = []
        else:
            self._custom = list(payload.get("custom") or [])
            self._detected = list(payload.get("detected") or [])
            self._diagnostics = list(payload.get("diagnostics") or [])
            self._status = (
                f"自定义 {len(self._custom)} · 检测 {len(self._detected)}"
            )
            if not self._custom and self._detected:
                self._active_column = 1
        self._index_custom = min(self._index_custom, max(0, len(self._filtered(self._custom)) - 1))
        self._index_detected = min(
            self._index_detected, max(0, len(self._filtered(self._detected)) - 1)
        )
        self._render_lists()
        self._render_status()
        self._render_diagnostics()

    def _switch_to(self, item: CatalogModel) -> None:
        self._switching = True
        self._status = f"正在切换到 {item.display_name or item.model_id}…"
        self._render_status()
        self._perform_switch(item)

    @work(thread=True, exclusive=True, group="model-picker-switch", exit_on_error=False)
    def _perform_switch(self, item: CatalogModel) -> None:
        try:
            token = _selection_token(item)
            message = ""

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
        items = self._filtered(self._custom if self._active_column == 0 else self._detected)
        index = self._index_custom if self._active_column == 0 else self._index_detected
        if not items:
            return None
        index = max(0, min(index, len(items) - 1))
        return items[index]

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

    def _render_lists(self) -> None:
        custom = self._filtered(self._custom)
        detected = self._filtered(self._detected)
        if custom:
            self._index_custom = max(0, min(self._index_custom, len(custom) - 1))
        if detected:
            self._index_detected = max(0, min(self._index_detected, len(detected) - 1))

        self.query_one("#list-custom", Static).update(
            self._render_column_text(custom, selected=self._index_custom, current=self._current_model())
        )
        self.query_one("#list-detected", Static).update(
            self._render_column_text(
                detected, selected=self._index_detected, current=self._current_model()
            )
        )
        self.query_one("#column-custom").set_class(self._active_column == 0, "active-column")
        self.query_one("#column-detected").set_class(self._active_column == 1, "active-column")

    def _render_column_text(
        self,
        items: Sequence[CatalogModel],
        *,
        selected: int,
        current: str,
    ) -> Text:
        if not items:
            return Text("（空）", style="#59676d")
        rendered = Text()
        for index, item in enumerate(items[:40]):
            is_selected = index == selected
            is_current = _is_current(item, current)
            marker = "●" if is_current else ("›" if is_selected else " ")
            style = "#00e5c3 bold" if is_selected else ("#d9e4e8" if is_current else "#8fa4ad")
            title = item.display_name or item.model_id
            meta = f"{item.profile_id or '-'} · {_protocol_label(item.protocol)}"
            caps = _capability_summary(item)
            line = f"{marker} {title}\n  {meta}"
            if caps:
                line += f"\n  {caps}"
            if item.matched_custom_key:
                line += f"\n  custom: {item.matched_custom_key}"
            rendered.append(line + "\n", style=style)
        remaining = len(items) - 40
        if remaining > 0:
            rendered.append(f"... 还有 {remaining} 项\n", style="#59676d")
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


def _protocol_label(protocol: str) -> str:
    mapping = {
        "openai_chat_completions": "Chat",
        "openai_responses": "Responses",
        "anthropic_messages": "Messages",
        "gemini_generate_content": "GenerateContent",
    }
    return mapping.get(protocol, protocol or "-")


def _capability_summary(item: CatalogModel) -> str:
    caps = item.capabilities
    bits: list[str] = []
    if item.context_window_tokens:
        tokens = item.context_window_tokens
        if tokens >= 1000:
            bits.append(f"{tokens // 1000}K")
        else:
            bits.append(str(tokens))
    if getattr(caps, "tools", False):
        bits.append("tools")
    if getattr(caps, "reasoning", False):
        bits.append("reasoning")
    if getattr(caps, "vision", False):
        bits.append("vision")
    return " · ".join(bits)


def _default_persist_selection(item: CatalogModel) -> str:
    if item.source == "custom" and item.key:
        path = save_active_model_ref(ActiveModelRef(source="custom", key=item.key, model_id=item.model_id))
        return f"已切换为自定义模型 {item.key}，并写入 {path}"
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
