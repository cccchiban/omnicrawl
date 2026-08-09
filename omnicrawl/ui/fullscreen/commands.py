"""全屏工作台的斜杠命令分派。

本模块只负责识别命令、调用 Agent 或既有通用命令处理器，并声明 UI 应如何
执行结果。它不依赖 Textual，也不会直接操作线程、输入锁、组件渲染或退出流程；
这些 UI 生命周期仍由 ``OmniCrawlApp`` 统一管理。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Callable, Literal, Protocol

from ...commands.slash import (
    format_memory_clean_result,
    format_mcp_status,
    format_plugins_status,
    format_skills_list,
    handle_approval_command,
    handle_model_command,
    handle_reasoning_command,
    handle_session_command,
    handle_subagent_task_command,
)


CommandExecution = Literal["immediate", "slow"]


class CommandAgent(Protocol):
    """全屏斜杠命令实际依赖的最小 Agent 协议。"""

    workspace_root: object

    def reset_conversation(self) -> None:
        """清空当前对话并开启新会话。"""

    def switch_workspace(self, workspace: str) -> object:
        """切换 Agent 当前工作区。"""

    def list_subagent_tasks(self) -> list[dict[str, Any]]:
        """列出当前会话可见的后台 SubAgent 任务。"""

    def get_subagent_task(self, task_id: str) -> dict[str, Any] | None:
        """读取当前会话中的后台 SubAgent 任务。"""

    def cancel_subagent_task(self, task_id: str) -> dict[str, Any]:
        """请求取消当前会话中的后台 SubAgent 任务。"""


@dataclass(frozen=True)
class CommandOutcome:
    """一次命令分派的 UI 无关结果。

    ``command`` 只在 ``execution == "slow"`` 时存在。调用方需要在线程 worker
    执行它，才能保持模型发现、MCP 连接和工作区重建不阻塞 Textual 主事件循环。
    ``open_model_picker`` 表示全屏 TUI 应打开双列模型选择界面。
    """

    handled: bool
    message: str | None = None
    execution: CommandExecution = "immediate"
    command: Callable[[], str | None] | None = None
    refresh_context: bool = False
    exit_requested: bool = False
    workspace_switch_requested: bool = False
    open_model_picker: bool = False
    model_picker_refresh: bool = False
    open_settings: bool = False

    def __post_init__(self) -> None:
        """防止调用方拿到互相矛盾的命令描述。"""

        if self.execution == "slow" and self.command is None and not self.open_model_picker:
            raise ValueError("慢命令必须提供可在线程中执行的 command")
        if self.execution == "immediate" and self.command is not None:
            raise ValueError("即时命令不应携带后台 command")
        if self.exit_requested and not self.handled:
            raise ValueError("退出请求必须被标记为已处理")
        if self.open_model_picker and self.open_settings:
            raise ValueError("一次命令不能同时打开模型和设置界面")
        if (self.open_model_picker or self.open_settings) and self.command is not None:
            raise ValueError("打开交互界面时不应再附带后台 command")


class CommandDispatcher:
    """将全屏 UI 输入映射为可由 UI 层执行的命令结果。

    构造函数允许注入既有 ``slash.py`` 处理器，便于纯单元测试；
    ``OmniCrawlApp`` 以延迟委托方式注入自身模块级入口，从而保留 monkeypatch
    兼容性。
    """

    _EXIT_WORDS = frozenset({"/quit", "退出", "结束", "再见"})

    def __init__(
        self,
        agent: CommandAgent,
        *,
        format_skills: Callable[[CommandAgent], str] = format_skills_list,
        format_mcp: Callable[[CommandAgent], str] = format_mcp_status,
        format_plugins: Callable[[CommandAgent], str] = format_plugins_status,
        format_memory_clean: Callable[[CommandAgent], str] = format_memory_clean_result,
        handle_session: Callable[[CommandAgent, str], str | None] = handle_session_command,
        handle_subagent_task: Callable[[CommandAgent, str], str | None] = (
            handle_subagent_task_command
        ),
        handle_model: Callable[[CommandAgent, str], str | None] = handle_model_command,
        handle_approval: Callable[[CommandAgent, str], str | None] = handle_approval_command,
        handle_reasoning: Callable[[CommandAgent, str], str | None] = handle_reasoning_command,
    ) -> None:
        self._agent = agent
        self._format_skills = format_skills
        self._format_mcp = format_mcp
        self._format_plugins = format_plugins
        self._format_memory_clean = format_memory_clean
        self._handle_session = handle_session
        self._handle_subagent_task = handle_subagent_task
        self._handle_model = handle_model
        self._handle_approval = handle_approval
        self._handle_reasoning = handle_reasoning

    def dispatch(self, text: str) -> CommandOutcome:
        """识别一条输入；普通自然语言返回 ``handled=False``。

        即时命令在此处保持原有调用时机。可能执行网络、进程或文件 I/O 的
        ``/mcp``、``/workspace`` 与 ``/model`` 则只生成惰性 callable，由 UI
        层在既有慢命令 worker 中调用。
        """

        stripped = text.strip()
        if stripped in self._EXIT_WORDS:
            return CommandOutcome(handled=True, exit_requested=True)
        if stripped == "/new":
            self._agent.reset_conversation()
            return CommandOutcome(
                handled=True,
                message="已开启新对话。",
                refresh_context=True,
            )
        if stripped == "/skills":
            return CommandOutcome(handled=True, message=self._format_skills(self._agent))
        if stripped == "/settings":
            return CommandOutcome(
                handled=True,
                message="打开设置面板",
                open_settings=True,
                refresh_context=True,
            )
        if stripped == "/mcp":
            return CommandOutcome(
                handled=True,
                message="正在读取 MCP 状态",
                execution="slow",
                command=lambda: self._format_mcp(self._agent),
            )
        if stripped == "/plugins":
            return CommandOutcome(
                handled=True,
                message=self._format_plugins(self._agent),
            )
        if stripped == "/memory:clean":
            return CommandOutcome(
                handled=True,
                message=self._format_memory_clean(self._agent),
            )
        if stripped == "/workspace" or stripped.startswith("/workspace "):
            return self._dispatch_workspace(stripped)

        session_message = self._handle_session(self._agent, text)
        if session_message is not None:
            return CommandOutcome(
                handled=True,
                message=session_message,
                refresh_context=True,
            )

        subagent_task_message = self._handle_subagent_task(self._agent, text)
        if subagent_task_message is not None:
            return CommandOutcome(handled=True, message=subagent_task_message)

        normalized = stripped.lower()
        if normalized == "/model":
            # 全屏 TUI 打开双列选择器；纯文本路径由 App 决定是否回退到列表输出。
            return CommandOutcome(
                handled=True,
                message="打开模型选择",
                open_model_picker=True,
                model_picker_refresh=False,
                refresh_context=True,
            )
        if normalized in {"/model --refresh", "/model -r", "/model refresh"}:
            return CommandOutcome(
                handled=True,
                message="刷新并打开模型选择",
                open_model_picker=True,
                model_picker_refresh=True,
                refresh_context=True,
            )
        if normalized.startswith("/model "):
            return CommandOutcome(
                handled=True,
                message="正在切换模型",
                execution="slow",
                command=lambda: self._handle_model(self._agent, text),
                refresh_context=True,
            )

        approval_message = self._handle_approval(self._agent, text)
        if approval_message is not None:
            return CommandOutcome(
                handled=True,
                message=approval_message,
                refresh_context=True,
            )

        reasoning_message = self._handle_reasoning(self._agent, text)
        if reasoning_message is not None:
            return CommandOutcome(
                handled=True,
                message=reasoning_message,
                refresh_context=True,
            )
        return CommandOutcome(handled=False)

    def _dispatch_workspace(self, text: str) -> CommandOutcome:
        """构造工作区切换结果，保留 UI 对 Monitor 生命周期的控制权。"""

        parts = text.split(None, 1)
        if len(parts) == 1 or not parts[1].strip():
            return CommandOutcome(
                handled=True,
                message=(
                    f"当前工作区：{self._agent.workspace_root}\n"
                    "用法：/workspace <新工作区路径>"
                ),
            )

        workspace = parts[1].strip()

        def switch_workspace() -> str:
            self._agent.switch_workspace(workspace)
            return f"已切换工作区：{self._agent.workspace_root}"

        return CommandOutcome(
            handled=True,
            message="正在切换工作区",
            execution="slow",
            command=switch_workspace,
            refresh_context=True,
            workspace_switch_requested=True,
        )


__all__ = ["CommandAgent", "CommandDispatcher", "CommandExecution", "CommandOutcome"]
