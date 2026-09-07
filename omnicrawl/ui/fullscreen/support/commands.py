"""全屏工作台的斜杠命令分派。

本模块只负责识别命令、调用 Agent 或既有通用命令处理器，并声明 UI 应如何
执行结果。它不依赖 Textual，也不会直接操作线程、输入锁、组件渲染或退出流程；
这些 UI 生命周期仍由 ``OmniCrawlApp`` 统一管理。
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import Any, Callable, Literal, Protocol

from ....commands.slash import (
    format_memory_clean_result,
    format_mcp_status,
    format_plugins_status,
    format_skills_list,
    handle_advisor_command,
    handle_approval_command,
    handle_mode_command,
    handle_reasoning_command,
    handle_model_command,
    handle_review_command,
    handle_session_command,
    handle_subagent_task_command,
)

_LOGGER = logging.getLogger(__name__)


CommandExecution = Literal["immediate", "slow"]


class CommandAgent(Protocol):
    """全屏斜杠命令实际依赖的最小 Agent 协议。"""

    workspace_root: object
    current_session_id: str

    def reset_conversation(self) -> None:
        """清空当前对话并开启新会话。"""

    def activate_mode(self, mode: str) -> str:
        """加载并启用主 Agent 模式模板。"""

    def switch_workspace(self, workspace: str) -> object:
        """切换 Agent 当前工作区。"""

    def list_subagent_tasks(self) -> list[dict[str, Any]]:
        """列出当前会话可见的后台 SubAgent 任务。"""

    def get_subagent_task(self, task_id: str) -> dict[str, Any] | None:
        """读取当前会话中的后台 SubAgent 任务。"""

    def cancel_subagent_task(self, task_id: str) -> dict[str, Any]:
        """请求取消当前会话中的后台 SubAgent 任务。"""

    def run_subagent_task(
        self,
        *,
        agent_type: str,
        description: str,
        prompt: str,
        on_subagent_event: Callable[[str, dict[str, Any]], None] | None = None,
    ) -> str:
        """同步运行一个 SubAgent 任务并返回最终文本结果。"""


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
        handle_approval: Callable[[CommandAgent, str], str | None] = handle_approval_command,
        handle_mode: Callable[[CommandAgent, str], str | None] = handle_mode_command,
        handle_reasoning: Callable[[CommandAgent, str], str | None] = handle_reasoning_command,
        handle_model: Callable[[CommandAgent, str], str | None] = handle_model_command,
        handle_advisor: Callable[[CommandAgent, str], str | None] = handle_advisor_command,
        handle_review: Callable[[CommandAgent, str], str | None] = handle_review_command,
    ) -> None:
        self._agent = agent
        self._format_skills = format_skills
        self._format_mcp = format_mcp
        self._format_plugins = format_plugins
        self._format_memory_clean = format_memory_clean
        self._handle_session = handle_session
        self._handle_subagent_task = handle_subagent_task
        self._handle_approval = handle_approval
        self._handle_mode = handle_mode
        self._handle_reasoning = handle_reasoning
        self._handle_model = handle_model
        self._handle_advisor = handle_advisor
        self._handle_review = handle_review

    def dispatch(self, text: str) -> CommandOutcome:
        """识别一条输入；普通自然语言返回 ``handled=False``。

        即时命令在此处保持原有调用时机。可能执行网络、进程或文件 I/O 的
        ``/mcp``、``/workspace`` 等则只生成惰性 callable，由 UI
        层在既有慢命令 worker 中调用。
        """

        stripped = text.strip()
        if stripped in self._EXIT_WORDS:
            return CommandOutcome(handled=True, exit_requested=True)
        if stripped == "/new":
            old_session_id = self._agent.current_session_id
            self._agent.reset_conversation()
            if old_session_id:
                message = f"已新开会话，旧会话：{old_session_id}"
            else:
                message = "已新开会话。"
            return CommandOutcome(
                handled=True,
                message=message,
                refresh_context=True,
                clear_conversation=True,
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
        mode_message = self._handle_mode(self._agent, text)
        if mode_message is not None:
            return CommandOutcome(
                handled=True,
                message=mode_message,
                refresh_context=True,
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
            normalized_session_command = stripped.casefold()
            undo_succeeded = (
                normalized_session_command == "/undo"
                and not session_message.startswith("会话回退失败：")
            )
            return CommandOutcome(
                handled=True,
                message=session_message,
                refresh_context=True,
                replay_conversation=undo_succeeded,
            )

        subagent_task_message = self._handle_subagent_task(self._agent, text)
        if subagent_task_message is not None:
            return CommandOutcome(handled=True, message=subagent_task_message)

        # /review 派生评审子 Agent，需要慢命令 worker 中执行模型循环。不显示
        # 静态占位提示；子代理对话（工具调用/结果）实时渲染到 │ 包裹的会话面板。
        if stripped == "/review" or stripped.startswith("/review "):
            return CommandOutcome(
                handled=True,
                message=None,
                working_status="正在评审",
                stream_subagent_conversation=True,
                execution="slow",
                command=lambda text=text: self._handle_review(self._agent, text),
            )

        normalized = stripped.lower()
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

        model_message = self._handle_model(self._agent, text)
        if model_message is not None:
            return CommandOutcome(
                handled=True,
                message=model_message,
                refresh_context=True,
            )

        advisor_message = self._handle_advisor(self._agent, text)
        if advisor_message is not None:
            return CommandOutcome(
                handled=True,
                message=advisor_message,
                refresh_context=True,
            )
        return CommandOutcome(handled=False)

    def is_immediate(self, text: str) -> bool:
        """判断命令是否可在 Agent 回合生成期间立即执行。

        仅允许纯 UI 操作或只读查询即时响应；会修改会话/回合状态、
        执行文件/网络 I/O 或切换工作区的命令必须保持 FIFO 排队，
        避免与后台回合 worker 并发。返回 True 的命令必然被
        ``dispatch`` 识别为 handled。
        """

        stripped = text.strip()
        if stripped in self._EXIT_WORDS:
            return True
        if stripped in {
            "/settings",   # 打开设置面板（纯 UI）
            "/skills",     # 只读 Skill 列表
            "/plugins",    # 只读插件状态
            "/approval",   # 只读查询审批模式（切换类命令会写配置，保持排队）
            "/sessions",   # 只读会话列表
            "/archives",   # 只读归档列表
            "/reasoning",  # 只读查询推理强度（切换类命令会写配置，保持排队）
            "/tasks",      # 只读子任务列表
            "/workspace",  # 只读当前工作区查询（切换走慢命令 worker）
        }:
            return True
        if stripped.startswith("/history "):
            return True  # 只读提示历史查询
        parts = stripped.split()
        if len(parts) == 2 and parts[0] == "/task" and parts[1].casefold() != "cancel":
            return True  # 只读任务详情；/task cancel 会取消任务，保持排队
        return False

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
            # 跨进程同步：把新工作区写回 config.toml，远程入口
            # （Telegram Bot）在任务开始前重读并跟随；失败不阻断切换。
            try:
                from ....config.core.workspace import save_workspace_root

                save_workspace_root(workspace)
            except Exception as exc:  # noqa: BLE001 - 持久化失败不阻断切换
                _LOGGER.warning("工作区持久化到 config.toml 失败：%s", exc)
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
