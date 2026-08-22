"""门面模块延迟解析兼容助手。

P3 重构把 ``OmniCrawlApp`` 拆分为子包后，测试/扩展仍会对
``omnicrawl.ui.fullscreen.<名称>`` 做 monkeypatch（如 ``OmniCrawlApp``、
``ModelPickerScreen``、``handle_approval_command``）。子包代码若在导入时
冻结引用（``from ..xxx import 名称``），patch 就不会生效；因此需要按名从
门面模块延迟解析。本模块提供统一入口，避免各子包各自引用 ``sys.modules``。
"""

from __future__ import annotations

import sys
from typing import Any

# 门面模块的完整点分名（本模块位于门面包内，__package__ 即门面包名）。
_FACADE_MODULE_NAME = __package__


def resolve_facade(name: str) -> Any:
    """从门面模块按名读取属性（调用时解析，patch 立即可见）。"""

    facade = sys.modules.get(_FACADE_MODULE_NAME)
    if facade is None:
        raise RuntimeError(f"门面模块未加载：{_FACADE_MODULE_NAME}")
    return getattr(facade, name)


__all__ = ["resolve_facade"]
