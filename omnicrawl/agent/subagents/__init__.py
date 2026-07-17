"""定义式 SubAgent 的内部实现。

当前支持同步/后台 fresh 任务，以及默认关闭的受控 Fork、任务级模型快照和安全生命周期事件；本包暂不作为稳定公共 API 导出。
"""

from .tasks import SubAgentTaskManager, SubAgentTaskSnapshot, SubAgentTaskSpec
from .definitions import (
    AgentDefinition,
    AgentDefinitionDiagnostic,
    AgentDefinitionError,
    AgentDefinitionRegistry,
)

__all__ = [
    "AgentDefinition",
    "AgentDefinitionDiagnostic",
    "AgentDefinitionError",
    "AgentDefinitionRegistry",
    "SubAgentTaskManager",
    "SubAgentTaskSnapshot",
    "SubAgentTaskSpec",
]
from . import worktree
