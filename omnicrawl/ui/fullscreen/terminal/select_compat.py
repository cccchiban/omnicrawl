"""Textual Select 挂载竞态兼容补丁。

规避 Textual 8.2.7 上游 bug（Textualize/textual#6581，PR #6599 未合并、
8.2.8 仍未修复）：``Select`` 收到 Mount 时其 compose 子组件
（``SelectOverlay`` / ``SelectCurrent`` 内的 ``#label``）可能尚未完成挂载，
基类 ``_on_mount`` 随即查询子组件会抛 ``NoMatches``，使整个 TUI 崩溃。

该竞态在 Windows 上间歇出现，批量挂载多个 Select（如设置面板同时
compose 6 个下拉框）时更容易触发。本项目在真实 Windows 终端打开
image_gen 等设置页时曾复现（No nodes match 'SelectOverlay'）。

补丁只把“初始选项渲染”推迟到子组件就绪之后，并给运行期查询补守卫，
不改变 Select 的公开行为；对未命中竞态的挂载路径零影响。
"""

from __future__ import annotations

import threading
from typing import Any

from textual.css.query import NoMatches
from textual.widgets import Static
from textual.widgets._select import Select, SelectCurrent

_ORIGINAL_ON_MOUNT = Select._on_mount
_ORIGINAL_SELECT_CURRENT_UPDATE = SelectCurrent.update
_patch_lock = threading.Lock()
_patch_applied = False


def _init_when_ready(self: Select) -> None:
    """子组件就绪后补齐初始选项渲染；仍缺失则静默放弃（不崩溃）。"""

    if not self.is_mounted or not self.is_attached or self._pruning:
        return
    try:
        self._setup_options_renderables()
        self._init_selected_option(self._value)
    except NoMatches:
        # 子组件仍未就绪：放弃本次初始化，避免把挂载流程拖垮。
        return


def _safe_on_mount(self: Select, _event: Any) -> None:
    """初始化选项；子组件未就绪（竞态窗口）时延迟到挂载完成后补齐。

    正常路径（子组件已就绪）同步完成，与基类时序一致；仅竞态路径
    通过 ``call_after_refresh`` 推迟，避免 NoMatches 拖垮整个 TUI。
    """

    try:
        self._setup_options_renderables()
        self._init_selected_option(self._value)
    except NoMatches:
        # overlay 或 #label 尚未就绪：整体推迟到下一轮刷新后补齐。
        self.call_after_refresh(_init_when_ready, self)


def _safe_select_current_update(self: SelectCurrent, label: Any) -> None:
    """更新选中标签；``#label`` 尚未挂载时跳过（挂载后刷新会自动补齐）。"""

    self.label = label
    self.has_value = label is not Select.NULL
    try:
        label_widget = self.query_one("#label", Static)
    except NoMatches:
        # #label 尚未挂载（SelectCurrent 已挂载、子节点未就绪的窗口期）。
        return
    if label is Select.NULL:
        label_widget.update(self.placeholder)
    else:
        label_widget.update(label)


def apply_textual_select_mount_patch() -> bool:
    """应用 Select 挂载竞态补丁（幂等，线程安全）。

    Returns:
        是否本次实际应用（重复调用返回 False）。
    """

    global _patch_applied
    with _patch_lock:
        if _patch_applied:
            return False
        Select._on_mount = _safe_on_mount
        SelectCurrent.update = _safe_select_current_update
        _patch_applied = True
        return True


def restore_textual_select() -> None:
    """恢复 Textual Select 原始方法（测试隔离/回滚用）。"""

    global _patch_applied
    with _patch_lock:
        Select._on_mount = _ORIGINAL_ON_MOUNT
        SelectCurrent.update = _ORIGINAL_SELECT_CURRENT_UPDATE
        _patch_applied = False


__all__ = ["apply_textual_select_mount_patch", "restore_textual_select"]
