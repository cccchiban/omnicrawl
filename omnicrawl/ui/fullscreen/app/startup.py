"""全屏工作台启动阶段参数。"""

from __future__ import annotations

from dataclasses import dataclass

from ....maintenance.version_check import current_version as _current_version


@dataclass(frozen=True)
class FullscreenStartup:
    """启动阶段提供给顶部上下文条的只读摘要。"""

    thinking_enabled: bool
    reasoning_effort: str
    approval_label: str
    workspace_label: str
    temp_label: str
    current_version: str = _current_version()
    version_check_enabled: bool = False
    # 真实入口在 Splash 阶段已完成所有准备；直接启动 App 的测试/扩展则
    # 保留旧行为，在 on_mount 中异步预热 MCP 并锁定输入。
    startup_ready: bool = False
    startup_messages: tuple[str, ...] = ()


__all__ = ["FullscreenStartup"]
