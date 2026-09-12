"""跨子系统共用的路径判断。

``is_relative_to`` 过去在 9 个模块里各写了一份同样的 ``relative_to`` +
``ValueError`` 判断（历史原因：更早的 Python 版本没有
``PurePath.is_relative_to``）。收拢到本模块后只保留一处实现；放在包根是
为了不让 ``extensions`` / ``workspace`` 等子系统为了路径判断而互相依赖。
"""

from __future__ import annotations

from pathlib import Path


def is_relative_to(path: Path, parent: Path) -> bool:
    """判断 path 是否位于 parent 之内（含与 parent 相等）。"""

    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


__all__ = ["is_relative_to"]
