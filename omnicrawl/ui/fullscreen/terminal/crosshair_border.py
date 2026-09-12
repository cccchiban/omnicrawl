"""Textual 自定义边框：设置页焦点栏的“准星”四角。

Textual 8.2.x 的边框字形来自内部固定表（``textual._border.BORDER_CHARS``），
CSS 只能选择内置类型，无法自定义角字符。这里注册一个额外的边框类型
``crosshair``：四个角为 ``⇘ ⇙ ⇖ ⇗``（全部指向框内，形似准星），四边沿用
内置 ``round`` 的 ``─`` / ``│``，用于设置页左右两栏标明键盘焦点所在的一栏。

补丁只新增一种边框类型，不改动任何内置字形；幂等且线程安全。
"""

from __future__ import annotations

import threading

from textual import _border
from textual._border import BORDER_CHARS, BORDER_LOCATIONS
from textual.css import constants

CROSSHAIR_BORDER = "crosshair"

# 三行分别为上、中、下；每行是左 / 中 / 右三列。
# 左上 ⇘ 与右上 ⇙ 指向下方（框内），左下 ⇗ 与右下 ⇖ 指向上方（框内）。
_CROSSHAIR_CHARS = (
    ("⇘", "─", "⇙"),
    ("│", " ", "│"),
    ("⇗", "─", "⇖"),
)

_patch_lock = threading.Lock()
_patch_applied = False


def apply_crosshair_border_patch() -> bool:
    """注册 ``crosshair`` 边框类型（幂等，线程安全）。

    Returns:
        是否本次实际注册（重复调用返回 False）。
    """

    global _patch_applied
    with _patch_lock:
        if _patch_applied:
            return False
        BORDER_CHARS[CROSSHAIR_BORDER] = _CROSSHAIR_CHARS
        BORDER_LOCATIONS[CROSSHAIR_BORDER] = BORDER_LOCATIONS["round"]
        # BORDER_LABEL_LOCATIONS 在 _border 导入时按 BORDER_LOCATIONS 派生，
        # 新类型需补齐，否则带标题的 crosshair 边框渲染会 KeyError。
        _border.BORDER_LABEL_LOCATIONS[CROSSHAIR_BORDER] = (
            BORDER_LOCATIONS["round"][0][1],
            BORDER_LOCATIONS["round"][2][1],
        )
        # CSS 解析按 VALID_BORDER 白名单校验边框名，必须同步登记；
        # 该集合被 _styles_builder 按引用导入，原地 add 即可生效。
        constants.VALID_BORDER.add(CROSSHAIR_BORDER)
        _patch_applied = True
        return True


def restore_crosshair_border() -> None:
    """撤销 ``crosshair`` 边框注册（测试隔离/回滚用）。"""

    global _patch_applied
    with _patch_lock:
        BORDER_CHARS.pop(CROSSHAIR_BORDER, None)
        BORDER_LOCATIONS.pop(CROSSHAIR_BORDER, None)
        _border.BORDER_LABEL_LOCATIONS.pop(CROSSHAIR_BORDER, None)
        constants.VALID_BORDER.discard(CROSSHAIR_BORDER)
        _patch_applied = False


__all__ = [
    "CROSSHAIR_BORDER",
    "apply_crosshair_border_patch",
    "restore_crosshair_border",
]
