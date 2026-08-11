"""SubAgent Phase 3 的私有执行快照类型。

Coordinator 只保存不可变的任务请求快照；真正的模型 Runtime 与父 Agent 可变
状态仍由 ``LocalToolAgent`` 持有。本模块故意不包含调度、工具执行或 Session
写入逻辑，避免 Fork 上下文与批次生命周期重新形成第二套 Agent Loop。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from ...config.llm import LLMConfig
from ...llm.registry import ModelDescriptor, ProviderProfile

if TYPE_CHECKING:
    from ...extensions.plugin_manager import PluginDispatchContext


FORK_BOILERPLATE = """<fork_boilerplate>
你是从 OmniCrawl 主 Agent 派生的工作进程，不是面向用户的主助手。
不可协商规则：
1. 不得再次创建 SubAgent。
2. 不得向用户提问；遇到需要产品或权限决策的问题应停止并报告。
3. 严格限制在分配任务范围内。
4. 只使用 Host 提供的工具，不得绕过审批、路径和安全策略。
5. 最终只返回结构化工作报告，不输出隐藏推理。
</fork_boilerplate>"""


@dataclass(frozen=True)
class SubAgentModelSnapshot:
    """任务创建时冻结的模型选择。

    ``llm_config`` 仅在进程内用于构造该任务的独立 Runtime；它绝不进入 Session、
    SSE、artifact、日志或公开 ToolResult。这样任务运行期间父 Agent 的模型
    切换只会影响后续任务，不会改变已创建的子任务模型身份。
    """

    selection: str
    llm_config: LLMConfig
    profile: ProviderProfile
    descriptor: ModelDescriptor


@dataclass(frozen=True)
class SubAgentExecutionContext:
    """单任务独立执行所需的不可变输入。

    ``fork_messages`` 是从父回合开始时构造的公开消息深拷贝，已经完成敏感信息
    脱敏。它不是父 ``_history`` 或当前 Agent Loop 的可变列表，因此子循环追加的
    tool call / tool result 不会写回父上下文。

    ``plugin_dispatch`` 是父线程在任务入队前冻结的只读 Plugin Hook 计划。
    子任务只能用它做 dispatch，不得调用父 PluginManager 的 begin_turn/end_turn。
    """

    context: str = "fresh"
    model_snapshot: SubAgentModelSnapshot | None = None
    fork_messages: tuple[dict[str, Any], ...] = ()
    parent_system_prompt: str = ""
    # fresh 任务在入队前冻结父 Skill 索引或当前活动 Skill 正文，避免后台任务
    # 在父 Agent 下一回合切换 Skill 后读取到新的可变状态。Fork 已从父公开消息继承。
    skill_context: str = ""
    plugin_dispatch: "PluginDispatchContext | None" = None
    # worktree 会话与工作区根由 Host 在 prepare 阶段绑定；execute 期间切换
    # WorkspaceTools 根目录，结束后收集 diff/branch 供父 Agent 审查应用。
    worktree_session: Any | None = None
    workspace_root: str = ""
    isolation: str = "shared"


__all__ = [
    "FORK_BOILERPLATE",
    "SubAgentExecutionContext",
    "SubAgentModelSnapshot",
]
