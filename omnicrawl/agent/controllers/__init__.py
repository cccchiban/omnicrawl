"""LocalToolAgent 领域拆分（类别包）。

每个类别一个子包，类别内不同功能放不同文件；
``core.LocalToolAgent`` 通过继承组合这些 Mixin。
"""
from .shared import (
    DEFAULT_TOOL_TIMEOUT_SECONDS,
    MAX_TOOL_TIMEOUT_SECONDS,
    TOOL_OUTPUT_INLINE_LIMIT_CHARS,
    TOOL_OUTPUT_BATCH_BUDGET_CHARS,
    TOOL_OUTPUT_ARCHIVED_PREVIEW_CHARS,
    SUBAGENT_LIFECYCLE_WAIT_SECONDS,
    SYSTEM_PROMPT_FILE,
    AGENTS_INSTRUCTIONS_FILE,
    _CONTEXT_OVERFLOW_RECOVERY_PROMPT,
    _CONTEXT_OVERFLOW_ERROR_MARKERS,
    _RATE_LIMIT_ERROR_MARKERS,
    _CONTINUE_LAST_TASK_TEXTS,
    AgentError,
    _ActiveTurnSnapshot,
    _READ_ONLY_UNDO_TOOLS,
    _REVERSIBLE_UNDO_TOOLS,
    _MEMORY_UNDO_EXEMPT_TOOLS,
    _ROUTER_UNDO_EXEMPT_TOOLS,
    _read_int_env,
    _validate_int_range,
    _validate_context_compaction_window,
    _unknown_tool_result,
    _tool_timeout_result,
    _execute_call_with_timeout,
)
from .session.control import SessionControlMixin
from .session.store import SessionStoreMixin
from .session.settings import SessionSettingsMixin
from .memory.stores import MemoryStoresMixin
from .workspace.switching import WorkspaceSwitchingMixin
from .workspace.toolbox import WorkspaceToolboxMixin
from .subagents.orchestration import SubAgentOrchestrationMixin
from .subagents.worktrees import SubAgentWorktreeMixin
from .turn.loop import TurnLoopMixin
from .turn.compaction import TurnCompactionMixin
from .tools.approval import ToolApprovalMixin
from .tools.building import ToolBuildingMixin
from .tools.implementations import ToolImplementationsMixin
from .tools.output import ToolOutputMixin
from .plugins import PluginHooksMixin
from .undo import UndoMixin

__all__ = [
    "DEFAULT_TOOL_TIMEOUT_SECONDS",
    "MAX_TOOL_TIMEOUT_SECONDS",
    "TOOL_OUTPUT_INLINE_LIMIT_CHARS",
    "TOOL_OUTPUT_BATCH_BUDGET_CHARS",
    "TOOL_OUTPUT_ARCHIVED_PREVIEW_CHARS",
    "SUBAGENT_LIFECYCLE_WAIT_SECONDS",
    "SYSTEM_PROMPT_FILE",
    "AGENTS_INSTRUCTIONS_FILE",
    "_CONTEXT_OVERFLOW_RECOVERY_PROMPT",
    "_CONTEXT_OVERFLOW_ERROR_MARKERS",
    "_RATE_LIMIT_ERROR_MARKERS",
    "_CONTINUE_LAST_TASK_TEXTS",
    "AgentError",
    "_ActiveTurnSnapshot",
    "_READ_ONLY_UNDO_TOOLS",
    "_REVERSIBLE_UNDO_TOOLS",
    "_MEMORY_UNDO_EXEMPT_TOOLS",
    "_ROUTER_UNDO_EXEMPT_TOOLS",
    "_read_int_env",
    "_validate_int_range",
    "_validate_context_compaction_window",
    "_unknown_tool_result",
    "_tool_timeout_result",
    "_execute_call_with_timeout",
    "SessionControlMixin",
    "SessionStoreMixin",
    "SessionSettingsMixin",
    "MemoryStoresMixin",
    "WorkspaceSwitchingMixin",
    "WorkspaceToolboxMixin",
    "SubAgentOrchestrationMixin",
    "SubAgentWorktreeMixin",
    "TurnLoopMixin",
    "TurnCompactionMixin",
    "ToolApprovalMixin",
    "ToolBuildingMixin",
    "ToolImplementationsMixin",
    "ToolOutputMixin",
    "PluginHooksMixin",
    "UndoMixin",
]
