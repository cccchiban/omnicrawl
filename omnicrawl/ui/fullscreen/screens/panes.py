"""右侧内嵌设置面板（pane）的公共基类与通用选项面板。

全屏设置页（``SettingsScreen``）把可独立设置的一级项列在左侧；选中后
右侧圆角框内挂载对应的二级面板。二级面板不再是自己 push 的
``ModalScreen``，而是继承 :class:`SettingsPane` 的普通 ``Widget``。

各模块内的 ``*SettingsScreen`` 保留为薄壳（ModalScreen），组合同一个
Pane 并把 Pane 的回调桥接回 ``dismiss``，从而对外协议与既有测试兼容；
全屏设置页右侧则直接挂载 Pane。两份入口共用同一份 Pane 逻辑。
"""

from __future__ import annotations

from typing import Any, Callable, Optional, Sequence

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Vertical
from textual.screen import ModalScreen
from textual.widget import Widget
from textual.widgets import Select, Static

from ..terminal.theme import terminal_css, terminal_select_css

_PANE_BASE_CSS = """
SettingsPane {
    height: 100%;
    width: 100%;
    padding: 0 1;
}
SettingsPane .pane-save-hint {
    height: 1;
    margin: 1 1 1 0;
    display: none;
    color: $terminal-green;
    text-style: bold;
    content-align: left middle;
}
"""


class SettingsPane(Widget, can_focus=True):
    """右侧内嵌设置面板基类。

    - 自绘列表类 pane：无内部可聚焦控件，pane 自身获得焦点后通过自身
      ``BINDINGS`` 响应方向键/Enter/Space。
    - 表单类 pane：内部有 ``Input``/``Select``/``Button`` 等可聚焦控件，
      pane 自身只注册 Esc/Ctrl+S 等非冲突按键，其余交给原生焦点链。
    - “返回上一级/左侧”调用 :meth:`request_back`；“保存完成”调用
      :meth:`commit`；“切换到其它二级面板/弹层”调用 :meth:`request_navigate`
      或 :meth:`request_modal`。都经宿主回调实现，不依赖 ModalScreen。
    """

    DEFAULT_CSS = terminal_css(_PANE_BASE_CSS)

    def __init__(self, agent: Any | None = None) -> None:
        super().__init__()
        self._agent = agent
        self._back_callback: Optional[Callable[[], None]] = None
        self._commit_callback: Optional[Callable[[Any], None]] = None
        self._navigate_callback: Optional[Callable[[str, Any], None]] = None
        self._modal_callback: Optional[Callable[[Any, Any], None]] = None

    def bind_pane_events(
        self,
        *,
        on_back: Callable[[], None],
        on_commit: Callable[[Any], None] | None = None,
        on_navigate: Callable[[str, Any], None] | None = None,
        on_modal: Callable[[Any, Any], None] | None = None,
    ) -> None:
        """宿主注入返回/提交/面板切换/弹层回调。"""
        self._back_callback = on_back
        if on_commit is not None:
            self._commit_callback = on_commit
        if on_navigate is not None:
            self._navigate_callback = on_navigate
        if on_modal is not None:
            self._modal_callback = on_modal

    def activate(self) -> None:
        """面板获得活动权：自绘列表类让 pane 自身聚焦，表单类可覆盖。"""
        self.focus()

    def request_back(self) -> None:
        """请求返回上一级（左侧列表或上级面板）。"""
        if self._back_callback is not None:
            self._back_callback()

    def commit(self, result: Any = None) -> None:
        """请求“保存完成”。"""
        if self._commit_callback is not None:
            self._commit_callback(result)

    def request_navigate(self, target: str, payload: Any = None) -> None:
        """请求宿主切换到其它二级面板（如 MCP Server 列表/编辑器）。"""
        if self._navigate_callback is not None:
            self._navigate_callback(target, payload)

    def request_modal(self, factory: Callable[[], ModalScreen[Any]], on_result: Callable[[Any], None]) -> None:
        """请求宿主 push 一个整屏弹层（如模型选择器）。"""
        if self._modal_callback is not None:
            self._modal_callback(factory, on_result)

    def compose(self) -> ComposeResult:
        yield from self.compose_pane()

    def compose_pane(self) -> ComposeResult:
        """子类实现面板内容。"""
        raise NotImplementedError

    def _can_refresh(self) -> bool:
        """面板仍完整挂在 DOM 上时才允许查询/更新子节点。

        Textual 的 ``is_mounted`` 卸载后不重置且不反映“正在拆除”，
        ``_pruning`` 在 remove_children 同步阶段即置位并持续到拆完，
        配合 ``is_attached`` 可覆盖卸载中/已卸载两个窗口，防止迟到的
        refresh（如 on_mount 的 call_after_refresh 排队后被切走）在
        子节点已清空时 query_one 抛 NoMatches 使整个 TUI 崩溃。
        """
        return self.is_mounted and not self._pruning and self.is_attached

    def on_mount(self) -> None:
        self.call_after_refresh(self.refresh_pane)

    def refresh_pane(self) -> None:
        """面板挂载后或需要重绘时调用；子类按需覆盖。"""

    def flash_save_hint(self, text: str = "设置已保存") -> None:
        """在按钮左侧短暂显示保存成功提示，约 2 秒后自动清除。

        面板须在 compose 中提供一个带 ``pane-save-hint`` class 的
        ``Static`` 占位；未提供或已卸载时静默跳过。
        """
        if not self.is_mounted:
            return
        hints = list(self.query(".pane-save-hint"))
        if not hints:
            return
        hint = hints[0]
        self._save_hint_widget = hint
        hint.update(text)
        hint.display = True
        if getattr(self, "_save_hint_timer", None) is not None:
            self._save_hint_timer.stop()
        self._save_hint_timer = self.set_timer(2.0, self._hide_save_hint)

    def _hide_save_hint(self) -> None:
        hint = getattr(self, "_save_hint_widget", None)
        if hint is None or not hint.is_attached:
            return
        hint.update("")
        hint.display = False


class SelectPane(SettingsPane):
    """“单个选项框”面板：一个 Select 下拉框，选择即应用保存。

    用于右侧只需一个离散设置的项（上下文长度、工具审批、推理强度、
    简单开关等），视觉与交互对齐 run_guard 等表单页的 ``Select``：
    回车/空格/↑↓ 展开候选，选中后立即保存，无需再按确认键。

    面板不重复标题（右侧标题由宿主显示）。``applier(value)`` 同步执行
    保存并返回状态文本；返回文本以“设置未完成：”开头视为失败，面板会
    把 Select 回滚到原值并展示错误状态。
    """

    BINDINGS = [
        Binding("escape", "cancel", "返回", priority=True),
    ]

    DEFAULT_CSS = terminal_css(
        _PANE_BASE_CSS
        + """
    #select-pane-body { height: 1fr; }
    #select-pane-status { height: 2; color: $terminal-text-secondary; margin-top: 2; }
    """
        + terminal_select_css()
    )

    def __init__(
        self,
        options: Sequence[tuple[str, Any]],
        current: Any,
        applier: Callable[[Any], str],
        *,
        agent: Any | None = None,
    ) -> None:
        super().__init__(agent=agent)
        self._options = list(options)
        self._applier = applier
        self._current = self._nearest(current)
        # 已应用的值：Select 挂载时会以初始值触发一次 Changed，若与已应用
        # 值相同则忽略（同一值重复选择不会再次触发 Changed）。
        self._applied = self._current
        self._status = ""
        self._busy = False

    def _nearest(self, value: Any) -> Any:
        """候选精确命中优先，否则取数值最接近的候选（可选项兜底首项）。"""
        for _label, option in self._options:
            try:
                if option == value:
                    return option
            except Exception:  # noqa: BLE001 - 个别取值不可比时继续
                continue
        try:
            base = float(value)
        except (TypeError, ValueError):
            return self._options[0][1]

        def _distance(option: Any) -> float:
            try:
                return abs(float(option) - base)
            except (TypeError, ValueError):
                return float("inf")

        return min((option for _label, option in self._options), key=_distance)

    def compose_pane(self) -> ComposeResult:
        with Vertical(id="select-pane-body"):
            yield Select(
                [(label, value) for label, value in self._options],
                value=self._current,
                allow_blank=False,
                id="select-pane-control",
                classes="choice-select",
            )
            yield Static(self._status, id="select-pane-status")

    def activate(self) -> None:
        self.query_one("#select-pane-control", Select).focus()

    def action_cancel(self) -> None:
        if not self._busy:
            self.request_back()

    def on_select_changed(self, event: Select.Changed) -> None:
        if self._busy or event.value is Select.NULL:
            return
        select = self.query_one("#select-pane-control", Select)
        previous = self._current
        # 挂载初始同步或用户重选同一值：值未真正变化，不触发保存。
        if event.value == self._applied:
            return
        self._busy = True
        try:
            status = self._applier(event.value)
            if status.startswith("设置未完成"):
                select.value = previous
                self._current = previous
                self._applied = previous
            else:
                self._current = event.value
                self._applied = event.value
            self._status = status
        finally:
            self._busy = False
        if self.is_mounted:
            self.query_one("#select-pane-status", Static).update(self._status)


__all__ = ["SettingsPane", "SelectPane"]
