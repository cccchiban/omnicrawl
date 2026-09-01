"""Agent 子系统核心：AgentConfig 与 LocalToolAgent 组合门面。

``LocalToolAgent`` 曾是 6000+ 行的上帝类；现已按领域拆分到
``controllers`` 包（session/memory/workspace/subagents/tools/turn 等类别），
本文件只保留配置模型、构造函数与组合继承，对外 API 与行为不变。
"""

from __future__ import annotations

import threading
from dataclasses import dataclass, field, replace
from pathlib import Path, PurePosixPath
from typing import Any, Callable, Mapping, Sequence
from .types import AskUserRequest
from .toolkit.windows_desktop import WindowsDesktopTools
from ..knowledge import KnowledgeBase, KnowledgeBaseError
from .subagents.coordinator import (
    SubAgentCoordinator,
    SubAgentExecutionResult,
    SubAgentPublicResult,
)
from .subagents.definitions import AgentDefinition, AgentDefinitionRegistry
from .subagents.worktree import (
    WorktreeError,
    WorktreeSession,
    apply_worktree_to_main,
    cleanup_worktree_session,
    collect_worktree_artifacts,
    create_worktree_session,
)
from ..approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_REVIEW,
    load_approval_mode,
    load_approval_review_model,
    normalize_approval_mode,
)
from ..config.features.context_compaction import (
    ContextCompactionConfig,
    load_context_compaction_config,
)
from ..config.features.run_guard import RunGuardConfig, load_run_guard_config
from ..config.features.agent_workspace import AgentWorkspaceConfig, load_agent_workspace_config
from ..config.features.image_gen import (
    ImageGenConfiguration,
    load_image_gen_configuration,
)
from ..config.features.tts import (
    TTSConfiguration,
    load_tts_configuration,
)
from ..config.features.subagents import (
    SubAgentConfig,
    SubAgentConfigError,
    load_subagent_config,
    validate_subagent_advanced_setting,
)
from ..config.features.tools import (
    ToolSwitchConfigError,
    load_disabled_tools,
    validate_tool_switch_name,
)
from ..config.models.vision import VisionConfiguration, load_vision_configuration
from ..llm import (
    LLMConfig,
    LLMError,
    ModelError,
    ModelErrorCode,
    ModelRuntimeManager,
    OpenAIResponseLLM,
    load_llm_config,
    normalize_reasoning_effort,
)
from ..mcp import MCPClientManager, MCPConfig, MCPConfigError, MCPToolMeta, load_mcp_config
from ..skill import SkillManager, SkillMatchResult
from ..temp_workspace import (
    AgentTempWorkspace,
    AgentTempWorkspaceConfig,
    AgentTempWorkspaceError,
    load_agent_temp_workspace_config,
)
from ..workspace_tools import (
    DEFAULT_COMMAND_TIMEOUT_SECONDS,
    MAX_COMMAND_TIMEOUT_SECONDS,
    WorkspaceToolError,
    WorkspaceTools,
)

from .controllers.shared import (
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
    _read_int_env,
    _validate_int_range,
    _validate_context_compaction_window,
    _unknown_tool_result,
    _tool_timeout_result,
    _execute_call_with_timeout,
)

from .controllers.session.control import SessionControlMixin
from .controllers.session.store import SessionStoreMixin
from .controllers.session.settings import SessionSettingsMixin
from .controllers.memory.stores import MemoryStoresMixin
from .controllers.workspace.switching import WorkspaceSwitchingMixin
from .controllers.workspace.toolbox import WorkspaceToolboxMixin
from .controllers.subagents.orchestration import SubAgentOrchestrationMixin
from .controllers.subagents.worktrees import SubAgentWorktreeMixin
from .controllers.turn.loop import TurnLoopMixin
from .controllers.turn.compaction import TurnCompactionMixin
from .controllers.tools.approval import ToolApprovalMixin
from .controllers.tools.building import ToolBuildingMixin
from .controllers.tools.implementations import ToolImplementationsMixin
from .controllers.tools.output import ToolOutputMixin
from .controllers.plugins import PluginHooksMixin
from .controllers.undo import UndoMixin


@dataclass
class AgentConfig:
    """本地 Agent 配置。

    workspace_root 约束所有文件工具的访问范围，避免模型误读或误写项目外路径。
    request_retry_count 控制空响应重试次数；request_timeout_seconds 控制每次模型请求超时。
    """

    llm: LLMConfig = field(default_factory=load_llm_config)
    workspace_root: Path = field(default_factory=lambda: Path.cwd())
    max_history_turns: int = 6
    request_retry_count: int = field(
        default_factory=lambda: _read_int_env("AGENT_REQUEST_RETRY_COUNT", 5, min_value=1, max_value=10)
    )
    request_timeout_seconds: int = field(
        default_factory=lambda: _read_int_env(
            "AGENT_REQUEST_TIMEOUT_SECONDS", 180, min_value=1, max_value=600
        )
    )
    skills_enabled: bool = True
    skill_paths: list[str] = field(default_factory=list)
    memory_enabled: bool = True
    # 是否渲染思考块（Markdown 渲染）：关闭只隐藏显示，思考内容仍照常产生与接收。
    show_thinking: bool = True
    memory_directory: str = ".omnicrawl/.oclmemory"
    session_enabled: bool = True
    session_directory: str = ".agent_sessions"
    resume_session_id: str = ""
    mcp_config: MCPConfig | None = None
    subagents: SubAgentConfig = field(default_factory=load_subagent_config)
    context_compaction: ContextCompactionConfig = field(
        default_factory=load_context_compaction_config
    )
    run_guard: RunGuardConfig = field(default_factory=load_run_guard_config)
    agent_workspace: AgentWorkspaceConfig = field(default_factory=load_agent_workspace_config)
    vision: VisionConfiguration = field(default_factory=load_vision_configuration)
    image_gen: ImageGenConfiguration = field(default_factory=load_image_gen_configuration)
    tts: TTSConfiguration = field(default_factory=load_tts_configuration)
    approval_mode: str = field(default_factory=load_approval_mode)
    # 自动审查使用的独立模型（approval.review_model）：为空时沿用主对话模型。
    # 审查请求与主对话隔离后，独立模型不影响主对话成本，且让审查决策
    # 不受主模型被提示词注入影响。
    approval_review_model: str = field(default_factory=load_approval_review_model)
    # 内置工具开关：默认除 powershell 外全部启用；配置 tools 段可覆盖。
    disabled_tools: frozenset[str] = field(default_factory=load_disabled_tools)
    workspace_detection_summary: str = ""
    temp_workspace: AgentTempWorkspaceConfig = field(
        default_factory=load_agent_temp_workspace_config
    )
    command_timeout_seconds: int = field(
        default_factory=lambda: _read_int_env(
            "AGENT_COMMAND_TIMEOUT_SECONDS",
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
            min_value=1,
            max_value=MAX_COMMAND_TIMEOUT_SECONDS,
        )
    )
    tool_timeout_seconds: int = field(
        default_factory=lambda: _read_int_env(
            "AGENT_TOOL_TIMEOUT_SECONDS",
            DEFAULT_TOOL_TIMEOUT_SECONDS,
            min_value=1,
            max_value=MAX_TOOL_TIMEOUT_SECONDS,
        )
    )

    def __post_init__(self) -> None:
        self.request_retry_count = _validate_int_range(
            "AGENT_REQUEST_RETRY_COUNT",
            self.request_retry_count,
            min_value=1,
            max_value=10,
        )
        self.request_timeout_seconds = _validate_int_range(
            "AGENT_REQUEST_TIMEOUT_SECONDS",
            self.request_timeout_seconds,
            min_value=1,
            max_value=600,
        )
        self.command_timeout_seconds = _validate_int_range(
            "AGENT_COMMAND_TIMEOUT_SECONDS",
            self.command_timeout_seconds,
            min_value=1,
            max_value=MAX_COMMAND_TIMEOUT_SECONDS,
        )
        self.tool_timeout_seconds = _validate_int_range(
            "AGENT_TOOL_TIMEOUT_SECONDS",
            self.tool_timeout_seconds,
            min_value=1,
            max_value=MAX_TOOL_TIMEOUT_SECONDS,
        )
        if not isinstance(self.memory_directory, str) or not self.memory_directory.strip():
            raise AgentError("memory_directory 必须是非空字符串。")
        if not isinstance(self.show_thinking, bool):
            raise AgentError("show_thinking 必须是布尔值。")
        if not isinstance(self.session_directory, str) or not self.session_directory.strip():
            raise AgentError("session_directory 必须是非空字符串。")
        if not isinstance(self.resume_session_id, str):
            raise AgentError("resume_session_id 必须是字符串。")
        self.resume_session_id = self.resume_session_id.strip()
        if self.resume_session_id and not self.session_enabled:
            raise AgentError("指定恢复会话时必须启用会话系统。")
        if not isinstance(self.temp_workspace, AgentTempWorkspaceConfig):
            raise AgentError("temp_workspace 必须是 AgentTempWorkspaceConfig。")
        if not isinstance(self.subagents, SubAgentConfig):
            raise AgentError("subagents 必须是 SubAgentConfig。")
        if not isinstance(self.context_compaction, ContextCompactionConfig):
            raise AgentError("context_compaction 必须是 ContextCompactionConfig。")
        if not isinstance(self.run_guard, RunGuardConfig):
            raise AgentError("run_guard 必须是 RunGuardConfig。")
        if not isinstance(self.vision, VisionConfiguration):
            raise AgentError("vision 必须是 VisionConfiguration。")
        if not isinstance(self.image_gen, ImageGenConfiguration):
            raise AgentError("image_gen 必须是 ImageGenConfiguration。")
        if not isinstance(self.tts, TTSConfiguration):
            raise AgentError("tts 必须是 TTSConfiguration。")
        # 百分比是配置关系而不是一次性 UI 计算结果：启动时按当前模型窗口
        # 重新换算，避免模型/窗口变化后仍沿用旧 Token 阈值。
        context_window_tokens = max(
            1,
            int(getattr(self.llm, "context_window_tokens", 128_000)),
        )
        compaction_percent = self.context_compaction.trigger_context_percent
        if compaction_percent is None:
            compaction_percent = round(
                self.context_compaction.trigger_context_tokens
                * 100
                / context_window_tokens
            )
            if compaction_percent > 0:
                self.context_compaction = replace(
                    self.context_compaction,
                    trigger_context_percent=compaction_percent,
                )
        if compaction_percent is not None:
            self.context_compaction = replace(
                self.context_compaction,
                trigger_context_tokens=max(
                    1,
                    context_window_tokens * compaction_percent // 100,
                ),
            )
        _validate_context_compaction_window(self.context_compaction, self.llm)
        if not isinstance(self.workspace_detection_summary, str):
            raise AgentError("workspace_detection_summary 必须是字符串。")
        if isinstance(self.disabled_tools, str):
            raise AgentError("disabled_tools 必须是字符串集合，不能是单个字符串。")
        try:
            self.disabled_tools = frozenset(self.disabled_tools)
        except TypeError:
            raise AgentError("disabled_tools 必须是字符串集合。")
        if not all(isinstance(name, str) and name for name in self.disabled_tools):
            raise AgentError("disabled_tools 的元素必须是非空字符串。")
        self.approval_mode = normalize_approval_mode(self.approval_mode)
        if not isinstance(self.approval_review_model, str):
            raise AgentError("approval_review_model 必须是字符串。")
        self.approval_review_model = self.approval_review_model.strip()

class LocalToolAgent(
    SessionControlMixin,
    SessionStoreMixin,
    SessionSettingsMixin,
    MemoryStoresMixin,
    WorkspaceSwitchingMixin,
    WorkspaceToolboxMixin,
    SubAgentOrchestrationMixin,
    SubAgentWorktreeMixin,
    TurnLoopMixin,
    TurnCompactionMixin,
    ToolApprovalMixin,
    ToolBuildingMixin,
    ToolImplementationsMixin,
    ToolOutputMixin,
    PluginHooksMixin,
    UndoMixin,
):
    """能在本地项目内读文件、检索、按确认执行写入/命令的简化 Agent Harness。

    参考 pi 的核心思想：Agent 不是一次问答，而是"模型 -> 工具 -> 观察 -> 下一轮模型"的循环。
    Provider 顶层注册当前 Agent 的所有可见工具（压缩描述与紧凑 Schema），模型直接原生
    调用真实工具名；Host 按完整目录二次校验参数、走审批并执行，执行后以 role=tool
    消息按 call_id 回传结果。
    """
    def __init__(
        self,
        config: AgentConfig | None = None,
        confirm: Callable[[str, dict[str, Any]], bool] | None = None,
        plugin_manager: Any | None = None,
        on_workspace_switched: Callable[[Path], Any] | None = None,
        on_plugin_settings_changed: Callable[[bool], Any] | None = None,
    ) -> None:
        self.config = config or AgentConfig()
        self.workspace_root = self.config.workspace_root.resolve()
        self._confirm = confirm or self._confirm_in_terminal
        self._ask_user_handler: Callable[[AskUserRequest], str | None] | None = None
        self._history: list[dict[str, str]] = []
        self._pending_user_text: str | None = None
        self._active_skills: list[SkillMatchResult] = []
        # 审查请求与主对话隔离，仅按线程保存最近一次模型请求的消息快照，
        # 供自动审查提取最近用户消息摘要（理解意图）。
        self._review_context_local = threading.local()
        self._closed = False
        self._closing = False
        self._close_callbacks: list[Callable[[], None]] = []
        # PluginManager 由进程级 PluginRuntime 在 Agent 创建前注入；缺省保持无插件兼容。
        self._plugin_manager = plugin_manager
        # 工作区切换成功后回调 PluginRuntime.switch_workspace，用于关闭旧 Worker 并重建。
        self._on_workspace_switched = on_workspace_switched
        # 设置面板切换 plugins.enabled 时复用进程级 PluginRuntime 的事务重建。
        self._on_plugin_settings_changed = on_plugin_settings_changed
        try:
            self._temp_workspace = AgentTempWorkspace(
                self.workspace_root,
                self.config.temp_workspace,
            )
            self._temp_workspace.ensure()
            self._temp_workspace.clean_if_due()
        except AgentTempWorkspaceError as exc:
            raise AgentError(str(exc)) from exc
        self._session_store = self._create_session_store() if self.config.session_enabled else None
        # 会话创建/恢复发生在 PluginManager 就绪之后，才能发布 session.* Hook。
        self._session_state = self._start_or_resume_session() if self._session_store is not None else None
        self._emit_session_lifecycle_hooks()
        self._project_store = self._create_project_store() if self._session_store is not None else None
        if self.config.memory_enabled:
            (
                self._project_memory_store,
                self._session_memory_store,
                self._user_memory_store,
            ) = self._create_memory_stores()
            # 旧内部字段保留为项目级别别名，兼容尚未迁移的调用方。
            self._memory_store = self._project_memory_store
        else:
            self._project_memory_store = None
            self._session_memory_store = None
            self._user_memory_store = None
            self._memory_store = None
        self._workspace_tools = WorkspaceTools(
            self.workspace_root,
            command_timeout_seconds=self.config.command_timeout_seconds,
            extra_protection_message=self._workspace_extra_protection_message,
        )
        # 跨项目工作知识库独立于工作区，惰性创建；仅在实际调用 kb_* 工具时初始化。
        self._knowledge_base: KnowledgeBase | None = None
        # 仅 Windows 注册桌面工具；非 Windows 不向模型暴露注定失败的工具定义。
        self._windows_desktop_tools: WindowsDesktopTools | None = (
            self._create_windows_desktop_tools()
            if WindowsDesktopTools.is_supported()
            else None
        )
        self._skill_manager: SkillManager | None = None

        if not self.config.llm.api_key.strip():
            raise AgentError("缺少 API Key，请在 config.toml 的 llm 配置中填写，或设置 OPENAI_API_KEY。")

        self._client: Any | None = None
        self._mcp_manager = self._create_mcp_manager()
        self._subagent_registry: AgentDefinitionRegistry | None = None
        self._subagent_coordinator: SubAgentCoordinator | None = None
        self._subagent_worktree_sessions: dict[str, WorktreeSession] = {}
        self._subagent_worktree_lock = threading.Lock()
        self._workspace_root_local = threading.local()
        # 子任务线程内的审批模式覆盖（如 gitMode=full 的评审角色强制自动批准）：
        # 只作用于当前 worker 线程，不影响父 Agent 或其他子任务。
        self._approval_mode_local = threading.local()
        # Coordinator worker 会并发上报事件；在同一锁内完成 Session 持久化和
        # 公开回调，确保两个消费者看到相同的安全 payload 与全局事件顺序。
        self._subagent_event_lock = threading.RLock()
        self._subagent_model_request_semaphore = (
            threading.BoundedSemaphore(self.config.subagents.model_request_concurrency)
            if self.config.subagents.enabled
            else None
        )
        if self.config.subagents.enabled:
            self._refresh_subagent_definitions()
            # 启动恢复发生在 Coordinator 创建之前；此处再导入一次跨进程终态快照。
            self.import_recovered_subagent_tasks()
        self._tools = self._build_tools()
        self._system_prompt_template = self._load_system_prompt_template()
        if self.config.skills_enabled:
            self._skill_manager = SkillManager()
            self._skill_manager.discover(
                cwd=self.workspace_root,
                extra_paths=self.config.skill_paths,
            )
        self._temp_workspace.start_scheduler()
