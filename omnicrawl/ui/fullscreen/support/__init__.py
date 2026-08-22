"""support 类别包：不依赖 Textual 的支持控制器与适配器。

本文件只做再导出，不存放业务逻辑：
- ``AgentTurnController``/``AgentTurnCallbacks`` 在 ``turns.py``；
- ``CommandDispatcher`` 在 ``commands.py``；
- ``MonitorStateAdapter`` 在 ``monitor.py``。
"""

from .commands import CommandAgent, CommandDispatcher, CommandExecution, CommandOutcome
from .monitor import (
    MonitorAgent,
    MonitorDisplayBatch,
    MonitorStateAdapter,
    format_monitor_display_batch,
)
from .turns import AgentTurnCallbacks, AgentTurnController, StreamAgent

__all__ = [
    "CommandAgent",
    "CommandDispatcher",
    "CommandExecution",
    "CommandOutcome",
    "MonitorAgent",
    "MonitorDisplayBatch",
    "MonitorStateAdapter",
    "format_monitor_display_batch",
    "AgentTurnCallbacks",
    "AgentTurnController",
    "StreamAgent",
]
