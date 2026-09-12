"""命令子系统：声明式命令框架 + 内置斜杠命令。

- :mod:`omnicrawl.commands.framework`：与业务无关的注册/解析/分发框架。
- :mod:`omnicrawl.commands.slash`：内置命令声明、格式化器与全局注册表。

业务代码推荐只依赖本包导出的框架类型与 :data:`REGISTRY`；``REGISTRY`` 采用
延迟导出，避免 ``commands`` 包初始化时提前拉起 ``slash``（及其配置依赖）。
"""

from __future__ import annotations

from typing import Any

from .framework import (
    UNHANDLED,
    Command,
    CommandContext,
    CommandParseError,
    CommandRegistry,
    CommandResult,
    CommandType,
)

__all__ = [
    "Command",
    "CommandContext",
    "CommandParseError",
    "CommandRegistry",
    "CommandResult",
    "CommandType",
    "REGISTRY",
    "UNHANDLED",
]


def __getattr__(name: str) -> Any:
    """延迟导出全局命令注册表。"""

    if name == "REGISTRY":
        from .slash import REGISTRY

        return REGISTRY
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
