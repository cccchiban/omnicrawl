"""全屏工作台的斜杠命令分派。

本模块是 :mod:`omnicrawl.commands.framework` 的 **交互层适配器**：解析、命令
匹配与执行都在 ``commands.slash`` 的注册表里，这里只把统一的
:class:`~omnicrawl.commands.framework.CommandResult` 翻译成 UI 能执行的
:class:`CommandOutcome`（慢命令交给 worker、退出、开设置面板等生命周期提示）。

它不依赖 Textual，也不会直接操作线程、输入锁、组件渲染或退出流程；这些 UI
生命周期仍由 ``OmniCrawlApp`` 统一管理。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Callable, Literal, Protocol

from ....commands.framework import CommandResult
from ....commands.slash import REGISTRY


CommandExecution = Literal["immediate", "slow"]


class CommandAgent(Protocol):
    """全屏斜杠命令实际依赖的最小 Agent 协议（供类型检查与阅读）。"""

    workspace_root: object
    current_session_id: str

    def reset_conversation(self) -> None:
        """清空当前对话并开启新会话。"""

    def activate_mode(self, mode: str) -> str:
        """加载并启用主 Agent 模式模板。"""

    def switch_workspace(self, workspace: str) -> object:
        """切换 Agent 当前工作区。"""


@dataclass(frozen=True)
class CommandOutcome:
    """一次命令分派的 UI 无关结果。

    ``command`` 只在 ``execution == "slow"`` 时存在。调用方需要在线程 worker
    执行它，才能保持模型发现、MCP 连接和工作区重建不阻塞 Textual 主事件循环。
    ``open_settings`` 表示全屏 TUI 应打开中文设置面板。
    ``clear_conversation`` 表示 UI 应先清空对话视图再显示命令消息。
    ``replay_conversation`` 表示命令更新了当前会话后，UI 应从最新事件流重建
    对话视图；这与只刷新 HUD 的 ``refresh_context`` 不同。
    """

    handled: bool
    message: str | None = None
    execution: CommandExecution = "immediate"
    command: Callable[[], str | None] | None = None
    refresh_context: bool = False
    exit_requested: bool = False
    workspace_switch_requested: bool = False
    open_settings: bool = False
    open_config_chat: bool = False
    clear_conversation: bool = False
    replay_conversation: bool = False
    # 慢命令运行时 HUD 状态行的文本（如 "正在评审"）；None 时沿用默认等待态。
    working_status: str | None = None
    # 慢命令期间把子代理事件渲染为 │ 包裹的对话面板（替代进度树），
    # 用于 /review 等派生评审流程。
    stream_subagent_conversation: bool = False

    def __post_init__(self) -> None:
        """防止调用方拿到互相矛盾的命令描述。"""

        if self.execution == "slow" and self.command is None:
            raise ValueError("慢命令必须提供可在线程中执行的 command")
        if self.execution == "immediate" and self.command is not None:
            raise ValueError("即时命令不应携带后台 command")
        if self.exit_requested and not self.handled:
            raise ValueError("退出请求必须被标记为已处理")
        if self.open_settings and self.command is not None:
            raise ValueError("打开交互界面时不应再附带后台 command")


def _resolve(result: CommandResult) -> str | None:
    """在 worker 线程中执行命令的延迟部分，返回可展示文本。"""

    resolved = result.resolve()
    if resolved.error:
        return resolved.error
    return resolved.message


class CommandDispatcher:
    """把全屏 UI 输入交给命令注册表，并翻译回 UI 可执行的调度结果。"""

    def __init__(self, agent: CommandAgent) -> None:
        self._agent = agent

    def dispatch(
        self,
        text: str,
        *,
        on_subagent_event: Callable[[str, dict[str, Any]], None] | None = None,
    ) -> CommandOutcome:
        """分派一条输入；普通自然语言返回 ``handled=False``。

        ``on_subagent_event`` 透传给声明了延迟执行的命令（如 ``/review``），
        让派生 SubAgent 的生命周期事件能实时渲染到对话区。
        """

        result = REGISTRY.dispatch(
            text,
            agent=self._agent,
            channel="tui",
            on_subagent_event=on_subagent_event,
        )
        if not result.handled:
            return CommandOutcome(handled=False)

        slow = result.deferred is not None
        return CommandOutcome(
            handled=True,
            message=result.message,
            execution="slow" if slow else "immediate",
            command=(lambda result=result: _resolve(result)) if slow else None,
            refresh_context=result.refresh_context,
            exit_requested=result.exit_requested,
            workspace_switch_requested=result.workspace_switch_requested,
            open_settings=result.open_settings,
            open_config_chat=getattr(result, "open_config_chat", False),
            clear_conversation=result.clear_conversation,
            replay_conversation=result.replay_conversation,
            working_status=result.working_status,
            stream_subagent_conversation=result.stream_subagent_conversation,
        )

    def is_immediate(self, text: str) -> bool:
        """判断命令是否可在 Agent 回合生成期间立即执行。

        由命令声明的 :class:`~omnicrawl.commands.framework.CommandType` 决定：
        纯界面动作与只读查询即时响应；会修改会话/配置状态或需要网络、进程、
        文件 I/O 的命令必须保持 FIFO 排队，避免与后台回合 worker 并发。
        """

        parsed = REGISTRY.parse(text)
        if parsed is None:
            return False
        return parsed.command.type.immediate


__all__ = ["CommandAgent", "CommandDispatcher", "CommandExecution", "CommandOutcome"]
