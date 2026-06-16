from __future__ import annotations

import json
import os
import platform
import re
import sys
import hashlib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from .approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_REVIEW,
    load_approval_mode,
    normalize_approval_mode,
)
from .llm import (
    LLMConfig,
    LLMError,
    OpenAIResponseLLM,
    VALID_REASONING_EFFORTS,
    load_llm_config,
    normalize_reasoning_effort,
)
from .memory import (
    MemoryStore,
    MemoryStoreError,
    MemoryWriteRequest,
    record_to_dict,
    search_result_to_dict,
)
from .mcp import MCPClientManager, MCPConfig, MCPConfigError, MCPToolMeta, load_mcp_config
from .session import (
    PromptHistoryEntry,
    SessionIndexEntry,
    SessionState,
    SessionStore,
    SessionStoreError,
)
from .skill import SkillManager, SkillMatchResult
from .temp_workspace import (
    AgentTempWorkspace,
    AgentTempWorkspaceConfig,
    AgentTempWorkspaceError,
    load_agent_temp_workspace_config,
)
from .workspace_tools import WorkspaceToolError, WorkspaceTools


SYSTEM_PROMPT_FILE = "system_prompt.md"
AGENTS_INSTRUCTIONS_FILE = "AGENTS.md"
TOOL_REVIEW_SYSTEM_PROMPT = (
    "你是本地 AI Agent 的工具调用安全审查器。"
    "review 模式下，Host 只会把疑似删除行为的工具调用交给你审查；非删除行为由 Host 自动放行。"
    "你只判断这一次工具调用是否可以自动批准，不执行工具，也不补写方案。"
    "请用严格 JSON 回复：{\"approve\": true/false, \"reason\": \"一句中文理由\"}。"
    "删除目标清晰、位于工作区内、影响范围明确时可以批准。"
    "当请求明显越界访问、读取密钥、破坏系统、递归或批量删除大量文件、修改真实生产数据、"
    "执行无法判断影响的危险删除命令，或参数不足以判断时，必须拒绝。"
    "如果工具调用经判断不是删除行为，可以批准并说明无需删除审批。"
)

_DELETE_COMMAND_PATTERN = re.compile(
    r"(?<![\w.-])(?:rm|rmdir|del|erase|rd|remove-item|ri|unlink|clean)"
    r"(?:\.exe|\.cmd|\.bat|\.ps1)?(?=\s|$|[;&|])",
    re.IGNORECASE,
)
_GIT_CLEAN_PATTERN = re.compile(r"(?<![\w.-])git(?:\.exe)?\s+clean(?=\s|$|[;&|])", re.IGNORECASE)
_FIND_DELETE_PATTERN = re.compile(r"(?<![\w.-])find(?:\.exe)?\b.*(?:\s-delete\b|\s-exec\s+rm\b)", re.IGNORECASE)
_DELETE_INTENT_PATTERN = re.compile(
    r"(^|[._:/\\-])(?:delete|del|erase|remove|rm|rmdir|unlink|删除|移除|清空)($|[._:/\\-])",
    re.IGNORECASE,
)
_DELETE_TEXT_INTENT_PATTERN = re.compile(
    r"(^|[\s._:/\\-])(?:delete|del|erase|remove|rm|rmdir|unlink)($|[\s._:/\\-])",
    re.IGNORECASE,
)
_CONTINUE_LAST_TASK_TEXTS = {
    "继续",
    "继续上次",
    "继续上一轮",
    "接着来",
    "接着做",
    "重试",
    "再试一次",
    "再试试",
    "retry",
    "continue",
}
_DELETE_DESCRIPTION_START_PATTERN = re.compile(
    r"^(?:delete|del|erase|remove|rm|rmdir|unlink)($|[\s._:/\\-])",
    re.IGNORECASE,
)
_DELETE_LOCALIZED_TERMS = ("删除", "移除", "清空")
_DELETE_INTENT_KEYS = {
    "action",
    "command",
    "cmd",
    "method",
    "mode",
    "op",
    "operation",
    "script",
    "verb",
}
_MCP_DELETE_INTENT_KEYS = _DELETE_INTENT_KEYS


def _runtime_environment_context(workspace_root: Path, workspace_detection_summary: str = "") -> str:
    """生成注入给模型的运行环境摘要。

    这里只暴露低敏、稳定且会影响工具选择的信息；不枚举完整环境变量，
    避免把 API Key、Token、代理配置等敏感值塞进模型上下文。
    """

    window_hint = _detect_agent_window_hint()
    command_shell_hint = _detect_command_shell_hint()
    terminal_hint = _detect_terminal_hint()
    lines = [
        "运行环境：",
        f"- 操作系统：{platform.system() or os.name} {platform.release()} ({platform.machine()})",
        f"- Python：{platform.python_version()}",
        f"- Python 可执行文件：{sys.executable}",
        f"- 工作区根目录：{workspace_root}",
        f"- 当前进程目录：{Path.cwd().resolve()}",
        f"- 路径分隔符：{os.sep}",
    ]
    if workspace_detection_summary.strip():
        lines.append(f"- 工作区检测：{workspace_detection_summary.strip()}")
    if window_hint:
        lines.append(f"- Agent 运行窗口：{window_hint}")
    if command_shell_hint:
        lines.append(f"- run_command 默认 Shell：{command_shell_hint}")
    if terminal_hint:
        lines.append(f"- 终端环境变量：{terminal_hint}")
    return "\n".join(lines)


def _detect_command_shell_hint() -> str:
    """检测 run_command 使用 shell=True 时最应遵循的命令语法。"""

    if os.name == "nt":
        comspec = os.getenv("COMSPEC", "").strip()
        shell = comspec or "cmd.exe"
        return f"{shell}（默认按 CMD 语法解析；PowerShell 语法需显式调用 powershell.exe -Command）"
    return os.getenv("SHELL", "").strip()


def _detect_agent_window_hint() -> str:
    """检测 Agent 所在的交互窗口或父进程链，帮助模型选择兼容命令。"""

    if os.name != "nt":
        shell = os.getenv("SHELL", "").strip()
        terminal = _detect_terminal_hint()
        if shell and terminal:
            return f"Shell={Path(shell).name}；终端={terminal}"
        return f"Shell={Path(shell).name}" if shell else terminal

    process_chain = _windows_process_name_chain()
    lowered_chain = [name.lower() for name in process_chain]
    shell_label = _windows_shell_label(lowered_chain)
    terminal_label = _windows_terminal_label(lowered_chain)

    if not shell_label and os.getenv("AI_VOICE_CHAT_IN_POWERSHELL") == "1":
        shell_label = "Windows PowerShell（由启动器创建）"

    parts: list[str] = []
    if terminal_label:
        parts.append(f"终端={terminal_label}")
    if shell_label:
        parts.append(f"Shell={shell_label}")
    if process_chain:
        parts.append(f"进程链={' <- '.join(process_chain[:8])}")
    return "；".join(parts) or "Windows 控制台（未识别具体 Shell）"


def _windows_shell_label(lowered_process_chain: list[str]) -> str:
    shell_labels = {
        "pwsh.exe": "PowerShell 7+",
        "powershell.exe": "Windows PowerShell",
        "cmd.exe": "CMD",
    }
    for name in lowered_process_chain:
        label = shell_labels.get(name)
        if label:
            return label
    return ""


def _windows_terminal_label(lowered_process_chain: list[str]) -> str:
    labels: list[str] = []
    if os.getenv("WT_SESSION", "").strip() or "windowsterminal.exe" in lowered_process_chain:
        labels.append("Windows Terminal")
    term_program = os.getenv("TERM_PROGRAM", "").strip()
    if term_program:
        labels.append(term_program)
    if "code.exe" in lowered_process_chain:
        labels.append("VS Code Terminal")
    if "conhost.exe" in lowered_process_chain:
        labels.append("Console Host")
    return " / ".join(dict.fromkeys(labels))


def _windows_process_name_chain(limit: int = 12) -> list[str]:
    """返回当前进程到祖先进程的 exe 名称链；失败时返回空列表。

    使用 Win32 Toolhelp API 避免依赖 psutil，也避免通过 shell 再启动子进程。
    """

    if os.name != "nt":
        return []

    try:
        import ctypes
        from ctypes import wintypes
    except ImportError:
        return []

    class PROCESSENTRY32W(ctypes.Structure):
        _fields_ = [
            ("dwSize", wintypes.DWORD),
            ("cntUsage", wintypes.DWORD),
            ("th32ProcessID", wintypes.DWORD),
            ("th32DefaultHeapID", ctypes.c_size_t),
            ("th32ModuleID", wintypes.DWORD),
            ("cntThreads", wintypes.DWORD),
            ("th32ParentProcessID", wintypes.DWORD),
            ("pcPriClassBase", wintypes.LONG),
            ("dwFlags", wintypes.DWORD),
            ("szExeFile", wintypes.WCHAR * 260),
        ]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    snapshot = kernel32.CreateToolhelp32Snapshot(0x00000002, 0)
    if snapshot == wintypes.HANDLE(-1).value:
        return []

    process_table: dict[int, tuple[int, str]] = {}
    try:
        entry = PROCESSENTRY32W()
        entry.dwSize = ctypes.sizeof(PROCESSENTRY32W)
        if not kernel32.Process32FirstW(snapshot, ctypes.byref(entry)):
            return []
        while True:
            process_table[int(entry.th32ProcessID)] = (
                int(entry.th32ParentProcessID),
                str(entry.szExeFile),
            )
            if not kernel32.Process32NextW(snapshot, ctypes.byref(entry)):
                break
    finally:
        kernel32.CloseHandle(snapshot)

    chain: list[str] = []
    seen: set[int] = set()
    pid = os.getpid()
    for _index in range(max(1, limit)):
        if pid in seen:
            break
        seen.add(pid)
        item = process_table.get(pid)
        if item is None:
            break
        parent_pid, name = item
        if name:
            chain.append(name)
        if parent_pid <= 0:
            break
        pid = parent_pid
    return chain


def _detect_terminal_hint() -> str:
    """返回终端类型线索，只使用常见非敏感变量名。"""

    hints: list[str] = []
    for name in ("WT_SESSION", "TERM_PROGRAM", "TERM"):
        value = os.getenv(name, "").strip()
        if value:
            hints.append(name if name == "WT_SESSION" else f"{name}={value}")
    return ", ".join(hints)


def _is_openai_gpt_model(model: str) -> bool:
    """只为 OpenAI GPT 系列模型启用官方 prompt_cache_key 参数。"""

    return model.startswith("gpt-") or model.startswith("chatgpt-") or bool(re.match(r"^o\d", model))


def _is_unsupported_prompt_cache_error(exc: Exception) -> bool:
    """兼容网关不认识 prompt_cache_key 时，自动移除该参数重试一次。"""

    message = str(exc).lower()
    return (
        "prompt_cache_key" in message
        and any(
            marker in message
            for marker in (
                "unknown",
                "unsupported",
                "unexpected",
                "unrecognized",
                "extra",
                "invalid",
                "not permitted",
            )
        )
    )


def _is_retryable_model_request_error(exc: Exception) -> bool:
    """识别请求建立阶段可直接重试的临时模型服务错误。"""

    status_code = getattr(exc, "status_code", None)
    if isinstance(status_code, int) and status_code in {408, 409, 500, 502, 503, 504}:
        return True

    message = str(exc).lower()
    return any(
        marker in message
        for marker in (
            "peer closed connection",
            "incomplete chunked read",
            "remote protocol error",
            "server disconnected",
            "connection reset",
            "connection aborted",
            "broken pipe",
            "timeout",
            "timed out",
            "readtimeout",
            "connecttimeout",
        )
    )


class AgentError(RuntimeError):
    """Agent 循环、工具调用或安全校验失败时抛出。"""


class _EmptyAgentReply(RuntimeError):
    """网关请求成功但没有返回可用文本，交由上层按策略重试。"""


class _RetryableAgentRequestError(RuntimeError):
    """模型请求遇到临时连接或服务端错误，可按请求重试策略重新发起。"""


@dataclass(frozen=True)
class ToolCall:
    """模型请求执行的一次工具调用。"""

    name: str
    arguments: dict[str, Any] = field(default_factory=dict)
    id: str = ""
    function_name: str = ""


@dataclass(frozen=True)
class ToolResult:
    """工具调用返回给模型的结构化结果。"""

    ok: bool
    output: str


@dataclass(frozen=True)
class AgentModelReply:
    """Chat Completions 一次回复的结构化结果。"""

    message: dict[str, Any]
    content: str
    tool_calls: list[ToolCall] = field(default_factory=list)
    reasoning: str = ""
    content_streamed: bool = False


@dataclass(frozen=True)
class ToolDefinition:
    """Agent 可用工具的说明与执行函数。"""

    name: str
    description: str
    argument_schema: str
    requires_confirmation: bool
    run: Callable[[dict[str, Any]], ToolResult]


def _read_int_env(name: str, default: int, *, min_value: int, max_value: int) -> int:
    """读取整数环境变量，并把配置错误转成 Agent 可捕获的中文错误。"""

    raw_value = os.getenv(name)
    if raw_value is None or raw_value.strip() == "":
        return default

    try:
        value = int(raw_value.strip())
    except ValueError as exc:
        raise AgentError(
            f"{name} 必须是 {min_value} 到 {max_value} 的整数，当前值：{raw_value}。"
        ) from exc

    return _validate_int_range(name, value, min_value=min_value, max_value=max_value)


def _validate_int_range(name: str, value: int, *, min_value: int, max_value: int) -> int:
    """校验整数范围，覆盖测试或调用方手动构造 AgentConfig 的情况。"""

    if isinstance(value, bool) or not isinstance(value, int):
        raise AgentError(f"{name} 必须是 {min_value} 到 {max_value} 的整数。")
    if value < min_value or value > max_value:
        raise AgentError(f"{name} 必须是 {min_value} 到 {max_value} 的整数，当前值：{value}。")
    return value


@dataclass
class AgentConfig:
    """本地 Agent 配置。

    workspace_root 约束所有文件工具的访问范围，避免模型误读或误写项目外路径。
    request_retry_count 控制空响应重试次数；request_timeout_seconds 控制每次模型请求超时。
    """

    llm: LLMConfig = field(default_factory=load_llm_config)
    workspace_root: Path = field(default_factory=lambda: Path.cwd())
    max_history_turns: int = 6
    max_tool_output_chars: int = 6000
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
    memory_directory: str = "memory"
    session_enabled: bool = True
    session_directory: str = ".agent_sessions"
    mcp_config: MCPConfig | None = None
    approval_mode: str = field(default_factory=load_approval_mode)
    workspace_detection_summary: str = ""
    temp_workspace: AgentTempWorkspaceConfig = field(
        default_factory=load_agent_temp_workspace_config
    )
    command_timeout_seconds: int = field(
        default_factory=lambda: _read_int_env(
            "AGENT_COMMAND_TIMEOUT_SECONDS", 120, min_value=1, max_value=300
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
            max_value=300,
        )
        if not isinstance(self.memory_directory, str) or not self.memory_directory.strip():
            raise AgentError("memory_directory 必须是非空字符串。")
        if not isinstance(self.session_directory, str) or not self.session_directory.strip():
            raise AgentError("session_directory 必须是非空字符串。")
        if not isinstance(self.temp_workspace, AgentTempWorkspaceConfig):
            raise AgentError("temp_workspace 必须是 AgentTempWorkspaceConfig。")
        if not isinstance(self.workspace_detection_summary, str):
            raise AgentError("workspace_detection_summary 必须是字符串。")
        self.approval_mode = normalize_approval_mode(self.approval_mode)


class LocalToolAgent:
    """能在本地项目内读文件、检索、按确认执行写入/命令的简化 Agent Harness。

    参考 pi 的核心思想：Agent 不是一次问答，而是"模型 -> 工具 -> 观察 -> 下一轮模型"的循环。
    当前实现使用 DeepSeek 官方 Chat Completions Tool Calls 协议：Host 通过
    tools 参数声明工具，模型通过 tool_calls 返回结构化调用，Host 执行后
    以 role=tool 消息回传结果。
    """

    _TOOL_NAME_ALIASES = {
        "listfiles": "list_files",
        "readfile": "read_file",
        "searchtext": "search_text",
        "replacetext": "replace_text",
        "writefile": "write_file",
        "runcommand": "run_command",
        "bb-browser.browser.tablist": "bb-browser.browser.tab_list",
        "bb-browser.browser.tabnew": "bb-browser.browser.tab_new",
        "bb-browser.browser.sitelist": "bb-browser.browser.site_list",
        "bb-browser.browser.siteinfo": "bb-browser.browser.site_info",
        "bb-browser.browser.siterun": "bb-browser.browser.site_run",
        "bb-browser.browser.type": "bb-browser.browser.type_text",
    }
    _ARGUMENT_NAME_ALIASES = {
        "cmd": "command",
        "caseSensitive": "case_sensitive",
        "casesensitive": "case_sensitive",
        "maxLines": "max_lines",
        "maxlines": "max_lines",
        "maxResults": "max_results",
        "maxresults": "max_results",
        "newText": "new_text",
        "newtext": "new_text",
        "oldText": "old_text",
        "oldtext": "old_text",
        "startLine": "start_line",
        "startline": "start_line",
        "tabId": "tab",
        "tabid": "tab",
        "timeoutSeconds": "timeout_seconds",
        "timeoutseconds": "timeout_seconds",
    }

    def __init__(
        self,
        config: AgentConfig | None = None,
        confirm: Callable[[str, dict[str, Any]], bool] | None = None,
    ) -> None:
        self.config = config or AgentConfig()
        self.workspace_root = self.config.workspace_root.resolve()
        self._confirm = confirm or self._confirm_in_terminal
        self._history: list[dict[str, str]] = []
        self._pending_user_text: str | None = None
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
        self._session_state = self._start_session() if self._session_store is not None else None
        self._memory_store = self._create_memory_store() if self.config.memory_enabled else None
        self._workspace_tools = WorkspaceTools(
            self.workspace_root,
            command_timeout_seconds=self.config.command_timeout_seconds,
            extra_protection_message=self._workspace_extra_protection_message,
        )
        self._skill_manager: SkillManager | None = None
        self._active_skills: list[SkillMatchResult] = []

        if not self.config.llm.api_key.strip():
            raise AgentError("缺少 API Key，请在 config.json 的 llm.api_key 中配置，或设置 OPENAI_API_KEY。")

        try:
            from openai import OpenAI
        except ImportError as exc:
            raise AgentError("缺少 openai 依赖，请先执行：pip install -r requirements.txt") from exc

        self._client = OpenAI(
            api_key=self.config.llm.api_key,
            base_url=self.config.llm.base_url,
        )
        self._mcp_manager = self._create_mcp_manager()
        self._tools = self._build_tools()
        self._system_prompt_template = self._load_system_prompt_template()
        if self.config.skills_enabled:
            self._skill_manager = SkillManager()
            self._skill_manager.discover(
                cwd=self.workspace_root,
                extra_paths=self.config.skill_paths,
            )
        self._temp_workspace.start_scheduler()

    @property
    def skill_manager(self) -> SkillManager | None:
        """公开 SkillManager 供 main.py 查询 /skills 列表。"""
        return self._skill_manager

    def format_mcp_status(self) -> str:
        """返回 MCP 子系统状态，供 `/mcp` 斜杠命令展示。"""

        return self._mcp_manager.format_status()

    def clean_memory(self) -> list[str]:
        """手动清理过期记忆，供 /memory:clean 命令调用。"""

        store = self._require_memory_store()
        try:
            return store.clean_expired_memories()
        except MemoryStoreError as exc:
            raise AgentError(str(exc)) from exc

    def reset_conversation(self) -> None:
        """开启新对话：清空对话历史并创建新会话，保留工具、记忆和 Skill 配置。"""

        self._history.clear()
        self._pending_user_text = None
        self._active_skills = []
        if self._session_store is not None:
            self._session_state = self._start_session()

    @property
    def current_session_id(self) -> str:
        """当前会话 ID；会话系统关闭时返回空字符串。"""

        state = getattr(self, "_session_state", None)
        return state.session_id if state is not None else ""

    def list_sessions(self, limit: int = 10) -> list[SessionIndexEntry]:
        """列出当前工作区最近会话，供 `/sessions` 展示。"""

        store = self._require_session_store()
        try:
            return store.list_sessions(workspace_root=self.workspace_root, limit=limit)
        except SessionStoreError as exc:
            raise AgentError(str(exc)) from exc

    def search_prompt_history(
        self,
        *,
        query: str = "",
        limit: int = 20,
        current_session_only: bool = False,
    ) -> list[PromptHistoryEntry]:
        """查询当前工作区的用户提示历史，供输入复用和 `/history` 展示。"""

        store = self._require_session_store()
        session_id = self.current_session_id if current_session_only else None
        try:
            return store.search_prompt_history(
                workspace_root=self.workspace_root,
                session_id=session_id,
                query=query,
                limit=limit,
            )
        except SessionStoreError as exc:
            raise AgentError(str(exc)) from exc

    def prompt_history_texts(self, limit: int = 100) -> list[str]:
        """返回按时间正序排列的提示文本，作为 TUI 上箭头历史种子。"""

        entries = self.search_prompt_history(limit=limit)
        return [entry.display for entry in reversed(entries)]

    def resume_session(self, session_id: str) -> SessionState:
        """恢复指定会话，并用转录消息重建 `_history`。"""

        store = self._require_session_store()
        try:
            state = store.load_session(session_id)
        except SessionStoreError as exc:
            raise AgentError(str(exc)) from exc
        if Path(state.workspace_root).resolve() != self.workspace_root.resolve():
            raise AgentError(
                "不能恢复其他工作区的会话："
                f"{state.workspace_root}"
            )
        self._session_state = state
        self._history = state.messages[-self.config.max_history_turns * 2 :]
        self._pending_user_text = None
        self._active_skills = []
        return state

    def close(self) -> None:
        """关闭 Agent 持有的外部资源，当前主要是 MCP stdio 子进程。"""

        manager = getattr(self, "_mcp_manager", None)
        if manager is not None:
            manager.close()
        temp_workspace = getattr(self, "_temp_workspace", None)
        if temp_workspace is not None:
            temp_workspace.close()

    @property
    def approval_mode(self) -> str:
        """当前工具审批模式，供 TUI 展示和斜杠命令切换。"""

        return self.config.approval_mode

    def set_approval_mode(self, mode: str) -> None:
        """运行时切换审批模式；持久化由调用方负责写入 config.json。"""

        self.config.approval_mode = normalize_approval_mode(mode)

    @property
    def current_model(self) -> str:
        """当前会话实际用于下一次请求的模型名称。"""

        return self.config.llm.model

    def set_model(self, model: str) -> None:
        """运行时切换模型；持久化由斜杠命令或 UI 调用方负责。"""

        model_id = model.strip()
        if not model_id:
            raise AgentError("模型 ID 不能为空。")
        self.config.llm.model = model_id

    @property
    def reasoning_effort(self) -> str:
        """当前推理强度，供 TUI / Qt 控件展示和切换。"""

        return self.config.llm.reasoning_effort

    def set_reasoning_effort(self, effort: str) -> str:
        """运行时切换推理强度；持久化由斜杠命令或 UI 调用方负责。"""

        try:
            normalized = normalize_reasoning_effort(effort)
        except LLMError as exc:
            raise AgentError(str(exc)) from exc
        self.config.llm.reasoning_effort = normalized
        self.config.llm.thinking_type = (
            "disabled" if normalized in {"none", "disabled"} else "enabled"
        )
        return normalized

    def set_confirm_handler(self, confirm: Callable[[str, dict[str, Any]], bool]) -> None:
        """替换确认交互，便于全屏 TUI 和行内 UI 使用不同展示方式。"""

        self._confirm = confirm

    def _create_memory_store(self) -> MemoryStore:
        """创建记忆存储，并把目录限制在工作区内。

        记忆目录由专用工具读写，普通文件工具会把 memory/ 视为受保护目录。
        这里不复用 _safe_path，是为了允许 MemoryStore 自己访问该受保护目录。
        """

        raw_directory = self.config.memory_directory.strip()
        candidate = Path(raw_directory)
        if not candidate.is_absolute():
            candidate = self.workspace_root / candidate
        resolved = candidate.resolve()
        if not self._is_relative_to(resolved, self.workspace_root):
            raise AgentError(f"记忆目录必须位于工作区内：{raw_directory}")
        return MemoryStore(resolved)

    def _create_session_store(self) -> SessionStore:
        """创建会话存储，并限制在工作区内。"""

        raw_directory = self.config.session_directory.strip()
        candidate = Path(raw_directory)
        if not candidate.is_absolute():
            candidate = self.workspace_root / candidate
        resolved = candidate.resolve()
        if not self._is_relative_to(resolved, self.workspace_root):
            raise AgentError(f"会话目录必须位于工作区内：{raw_directory}")
        store = SessionStore(resolved)
        try:
            store.ensure()
        except SessionStoreError as exc:
            raise AgentError(str(exc)) from exc
        return store

    def _start_session(self) -> SessionState:
        store = self._require_session_store()
        try:
            return store.start_session(self.workspace_root)
        except SessionStoreError as exc:
            raise AgentError(str(exc)) from exc

    def _create_mcp_manager(self) -> MCPClientManager:
        """加载并初始化 MCP Client Manager。

        MCP 是增量能力：配置关闭时不影响内置工具；启用后单个 Server 失败也只进入
        诊断信息，保留基础对话和内置工具可用性。配置本身不合法则阻止启动，避免用户
        误以为 MCP 已经按预期暴露能力。
        """

        try:
            mcp_config = self.config.mcp_config or load_mcp_config()
            manager = MCPClientManager(
                mcp_config,
                workspace_root=self.workspace_root,
                approval_mode_getter=lambda: self.config.approval_mode,
            )
            if mcp_config.enabled:
                manager.discover()
            return manager
        except MCPConfigError as exc:
            raise AgentError(str(exc)) from exc

    def run_stream(
        self,
        user_text: str,
        on_delta: Callable[[str], None],
        on_status: Callable[[str], None] | None = None,
        on_tool_start: Callable[[int, ToolCall], None] | None = None,
        on_tool_result: Callable[[ToolCall, ToolResult], None] | None = None,
        on_token_usage: Callable[[int, int, int], None] | None = None,
        on_protocol_wait: Callable[[], None] | None = None,
        on_retry_status: Callable[[str], None] | None = None,
    ) -> str:
        """执行一轮 Agent 任务，并把最终回答交给 on_delta 输出。

        工具调用过程通过 on_status 报告给命令行；最终回答仍走 on_delta，让现有 TTS
        分句播报逻辑可以继续复用。
        """

        text = user_text.strip()
        if not text:
            raise AgentError("用户输入为空，无法发送给 Agent。")

        status = on_status or (lambda _message: None)
        report_tool_start = on_tool_start or (lambda _step, _tool_call: None)
        report_tool_result = on_tool_result or (lambda _tool_call, _result: None)
        report_token_usage = on_token_usage or (
            lambda _input_tokens, _output_tokens, _cached_input_tokens: None
        )
        _report_protocol_wait = on_protocol_wait or (lambda: None)
        report_retry_status = on_retry_status or status

        text = self._apply_skill_command(text, status)
        pending_text = getattr(self, "_pending_user_text", None)
        text = self._resolve_continue_request(text)
        self._pending_user_text = pending_text or text
        self._append_prompt_history(text)
        self._append_session_event("user_message", {"content": text})
        working_messages = [
            *self._project_instructions_messages(),
            *self._history,
            {"role": "user", "content": text},
        ]

        try:
            all_reasoning_parts: list[str] = []
            step = 1
            while True:
                reply = self._request_agent_reply(
                    working_messages,
                    on_delta,
                    report_token_usage,
                    _report_protocol_wait,
                    report_retry_status,
                )
                if reply.reasoning:
                    all_reasoning_parts.append(reply.reasoning)

                if not reply.tool_calls:
                    final_reply = reply.content.strip()
                    if final_reply and not reply.content_streamed:
                        on_delta(final_reply)
                    combined_reasoning = "\n".join(all_reasoning_parts)
                    self._append_history(text, final_reply, combined_reasoning)
                    self._append_session_event("assistant_message", {"content": final_reply})
                    self._pending_user_text = None
                    return final_reply

                working_messages.append(reply.message)
                for raw_tool_call in reply.tool_calls:
                    tool_call = self._normalize_tool_call(raw_tool_call)
                    self._append_session_event(
                        "tool_call_requested",
                        {
                            "tool": tool_call.name,
                            "arguments": tool_call.arguments,
                            "tool_call_id": tool_call.id,
                            "function_name": tool_call.function_name,
                        },
                    )
                    tool = self._tools.get(tool_call.name)
                    if tool is None:
                        tool_result = ToolResult(
                            ok=False,
                            output=f"未知工具：{tool_call.name}。可用工具：{', '.join(self._tools)}",
                        )
                    else:
                        tool_result = self._run_tool(
                            tool,
                            tool_call.arguments,
                            on_start=lambda step=step, tool_call=tool_call: report_tool_start(
                                step,
                                tool_call,
                            ),
                        )
                    report_tool_result(tool_call, tool_result)
                    self._append_session_event(
                        "tool_result",
                        {
                            "tool": tool_call.name,
                            "tool_call_id": tool_call.id,
                            "ok": tool_result.ok,
                            "output": tool_result.output,
                        },
                    )
                    working_messages.append(self._tool_result_message(tool_call, tool_result))
                    step += 1
                status("")  # 通知调用方重新启动等待动画
        except Exception as exc:
            self._append_session_event(
                "session_interrupted",
                {
                    "user_text": text,
                    "reason": str(exc),
                },
            )
            raise

    def _apply_skill_command(self, text: str, status: Callable[[str], None]) -> str:
        """处理 /skill:name，并在每轮开始时清空上一轮手动 Skill 注入。"""

        self._active_skills = []
        if self._skill_manager is None or not text.startswith("/skill:"):
            return text

        parts = text.split(None, 1)
        skill_name = parts[0][len("/skill:") :].strip()
        skill = self._skill_manager.match_by_name(skill_name)
        if skill is None:
            status(f"未找到 Skill：{skill_name}")
            available = ", ".join(m.name for m in self._skill_manager.list_all()) or "无"
            return f"Skill「{skill_name}」不存在。当前可用的 Skill：{available}"

        self._active_skills = [
            SkillMatchResult(skill=skill, score=1.0, reason=f"手动调用：{skill_name}")
        ]
        status(f"已加载 Skill：{skill_name}")
        return parts[1] if len(parts) > 1 else f"请执行 {skill_name} 技能。"

    def _resolve_continue_request(self, text: str) -> str:
        """把短“继续/重试”恢复为上一轮未完成的真实用户任务。"""

        if not self._is_continue_last_task_request(text):
            return text

        pending_text = (getattr(self, "_pending_user_text", None) or "").strip()
        if not pending_text:
            return text

        return (
            "继续上一轮未完成任务。上一轮任务内容如下，请不要要求用户重复说明，"
            "直接基于这个任务继续执行或重试：\n"
            f"{pending_text}"
        )

    @staticmethod
    def _is_continue_last_task_request(text: str) -> bool:
        normalized = re.sub(r"[\s，。.!！?？]+", "", text.strip()).lower()
        return normalized in _CONTINUE_LAST_TASK_TEXTS

    def _project_instructions_messages(self) -> list[dict[str, str]]:
        """构造每次请求最前方的项目规范上下文消息。

        这个消息不写入 `_history`，但会在每次发起模型请求时放在 messages 列表开头。
        对无服务端会话状态的 Chat Completions 调用来说，模型只能看到本次请求携带的
        messages；因此项目规范必须随每次请求发送一次，但不能累积进本地历史，否则
        多轮对话会出现多份重复 AGENTS.md。
        """

        instructions = self._load_agents_instructions()
        if not instructions:
            return []
        return [
            {
                "role": "user",
                "content": (
                    "<project_instructions file=\"AGENTS.md\">\n"
                    f"{instructions}\n"
                    "</project_instructions>"
                ),
            }
        ]

    def _load_agents_instructions(self) -> str:
        """读取工作区根目录的 AGENTS.md；缺失时保持原 user prompt。"""

        path = self.workspace_root / AGENTS_INSTRUCTIONS_FILE
        if not path.is_file():
            return ""
        try:
            return path.read_text(encoding="utf-8").strip()
        except UnicodeDecodeError as exc:
            raise AgentError(f"{AGENTS_INSTRUCTIONS_FILE} 必须是 UTF-8 文本。") from exc
        except OSError as exc:
            raise AgentError(f"读取 {AGENTS_INSTRUCTIONS_FILE} 失败：{exc}") from exc

    def _request_agent_reply(
        self,
        messages: list[dict[str, Any]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int, int], None],
        on_protocol_wait: Callable[[], None],
        on_retry_status: Callable[[str], None],
    ) -> AgentModelReply:
        """请求模型给出下一步：要么返回 tool_calls，要么输出最终回答。"""

        last_retryable_error: Exception | None = None
        for attempt in range(1, self.config.request_retry_count + 1):
            try:
                return self._request_agent_reply_once(
                    messages,
                    on_delta,
                    on_token_usage,
                    on_protocol_wait,
                )
            except _EmptyAgentReply as exc:
                last_retryable_error = exc
                if attempt < self.config.request_retry_count:
                    continue
                raise AgentError(
                    f"Agent 连续 {self.config.request_retry_count} 次返回空响应，已停止本轮请求。"
                ) from exc
            except _RetryableAgentRequestError as exc:
                last_retryable_error = exc
                if attempt < self.config.request_retry_count:
                    on_retry_status(
                        f"模型请求中断，正在重试 {attempt + 1}/{self.config.request_retry_count}：{exc}"
                    )
                    continue
                raise AgentError(f"Agent 模型请求中断：{exc}") from exc

        raise AgentError(
            f"Agent 连续 {self.config.request_retry_count} 次返回空响应，已停止本轮请求。"
        ) from last_retryable_error

    def _request_agent_reply_once(
        self,
        messages: list[dict[str, Any]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int, int], None],
        on_protocol_wait: Callable[[], None],
    ) -> AgentModelReply:
        """执行一次 DeepSeek Chat Completions 流式工具调用请求。

        使用 stream=True 实现增量文本推送，提升 TUI 实时反馈体验；
        同时累积 tool_calls 增量块，流结束后统一解析为结构化 ToolCall 列表。
        """

        system_prompt = self._system_prompt()
        request_kwargs: dict[str, Any] = {
            "model": self.config.llm.model,
            "messages": [{"role": "system", "content": system_prompt}, *messages],
            "tools": self._chat_completion_tools(),
            "tool_choice": "auto",
            "stream": True,
            "extra_body": self._build_extra_body(),
            "timeout": self.config.request_timeout_seconds,
        }
        prompt_cache_key = self._build_prompt_cache_key(system_prompt, messages)
        if prompt_cache_key:
            request_kwargs["prompt_cache_key"] = prompt_cache_key

        try:
            stream = self._client.chat.completions.create(**request_kwargs)
        except Exception as exc:
            if "prompt_cache_key" in request_kwargs and _is_unsupported_prompt_cache_error(exc):
                request_kwargs.pop("prompt_cache_key", None)
                try:
                    stream = self._client.chat.completions.create(**request_kwargs)
                except Exception as retry_exc:
                    raise AgentError(
                        f"Agent 请求失败：{OpenAIResponseLLM.format_request_error(retry_exc)}"
                    ) from retry_exc
            elif _is_retryable_model_request_error(exc):
                raise _RetryableAgentRequestError(OpenAIResponseLLM.format_request_error(exc)) from exc
            else:
                raise AgentError(
                    f"Agent 请求失败：{OpenAIResponseLLM.format_request_error(exc)}"
                ) from exc

        content_parts: list[str] = []
        reasoning_parts: list[str] = []
        tool_call_delta_buffers: dict[int, dict[str, Any]] = {}
        latest_usage: tuple[int, int, int] | None = None
        has_streamed_visible = False
        protocol_wait_sent = False

        try:
            for event in stream:
                usage = OpenAIResponseLLM.extract_token_usage(event)
                if usage is not None:
                    latest_usage = usage

                delta = self._extract_stream_delta(event)
                if delta is None:
                    continue

                delta_content = LocalToolAgent._read_attr_or_key(delta, "content")
                if isinstance(delta_content, str) and delta_content:
                    content_parts.append(delta_content)
                    on_delta(delta_content)
                    has_streamed_visible = True

                delta_reasoning = LocalToolAgent._read_attr_or_key(delta, "reasoning_content")
                if isinstance(delta_reasoning, str):
                    reasoning_parts.append(delta_reasoning)

                tc_deltas = LocalToolAgent._read_attr_or_key(delta, "tool_calls")
                if isinstance(tc_deltas, list) and tc_deltas:
                    if has_streamed_visible and not protocol_wait_sent:
                        on_protocol_wait()
                        protocol_wait_sent = True
                    self._accumulate_tool_call_deltas(tc_deltas, tool_call_delta_buffers)
        except Exception as exc:
            raise _RetryableAgentRequestError(OpenAIResponseLLM.format_request_error(exc)) from exc

        if latest_usage is not None:
            on_token_usage(*latest_usage)

        content = "".join(content_parts)
        reasoning = "".join(reasoning_parts).strip()

        tool_calls = self._build_tool_calls_from_deltas(tool_call_delta_buffers)

        if not content.strip() and not tool_calls:
            raise _EmptyAgentReply("Agent 返回内容为空，且未返回工具调用。")

        message = self._assistant_tool_call_message({}, content, tool_calls, reasoning)

        return AgentModelReply(
            message=message,
            content=content,
            tool_calls=tool_calls,
            reasoning=reasoning,
            content_streamed=has_streamed_visible,
        )

    def _build_prompt_cache_key(
        self,
        system_prompt: str,
        messages: list[dict[str, Any]],
    ) -> str:
        """为 GPT/OpenAI 请求提供稳定缓存路由 key。

        Prompt caching 依赖稳定的长前缀。这里用系统提示词和项目级说明生成短 hash，
        让同一项目、同一工具/Skill/AGENTS 配置尽量落到同一缓存路由；历史和当前
        用户输入不参与 hash，避免每轮对话都换 key。
        """

        model = self.config.llm.model.strip().lower()
        if not _is_openai_gpt_model(model):
            return ""

        stable_parts = [self.config.llm.model.strip(), str(self.workspace_root), system_prompt]
        if messages and messages[0].get("content", "").startswith("<project_instructions"):
            stable_parts.append(messages[0]["content"])
        digest = hashlib.sha256("\n\n".join(stable_parts).encode("utf-8")).hexdigest()[:32]
        return f"local-agent-{digest}"

    def _build_extra_body(self) -> dict[str, Any]:
        """构造网关扩展参数；根据 reasoning_effort 决定是否启用思考模式。"""

        thinking_type = "enabled" if self.config.llm.thinking_enabled else "disabled"
        body: dict[str, Any] = {"thinking": {"type": thinking_type}}
        if self.config.llm.thinking_enabled and self.config.llm.reasoning_effort:
            if self.config.llm.reasoning_effort in VALID_REASONING_EFFORTS:
                body["reasoning_effort"] = self.config.llm.reasoning_effort
        return body

    @staticmethod
    def _parse_tool_arguments(raw_arguments: Any) -> dict[str, Any]:
        if isinstance(raw_arguments, dict):
            return raw_arguments
        if isinstance(raw_arguments, str) and raw_arguments.strip():
            try:
                parsed = json.loads(raw_arguments)
            except json.JSONDecodeError:
                return {}
            return parsed if isinstance(parsed, dict) else {}
        return {}

    @staticmethod
    def _read_attr_or_key(value: Any, key: str) -> Any:
        if value is None:
            return None
        attr = getattr(value, key, None)
        if attr is not None:
            return attr
        if isinstance(value, dict):
            return value.get(key)
        if hasattr(value, "model_dump"):
            data = value.model_dump()
            return data.get(key) if isinstance(data, dict) else None
        return None

    @staticmethod
    def _extract_stream_delta(event: Any) -> Any | None:
        """从流式事件中提取 choices[0].delta，兼容 SDK 模型与字典。"""

        choices = getattr(event, "choices", None)
        if isinstance(choices, list) and choices:
            delta = getattr(choices[0], "delta", None)
            if delta is not None:
                return delta
            first = choices[0]
            if isinstance(first, dict):
                return first.get("delta")
        elif isinstance(event, dict):
            choices_data = event.get("choices")
            if isinstance(choices_data, list) and choices_data:
                first = choices_data[0]
                if isinstance(first, dict):
                    return first.get("delta")
        return None

    @classmethod
    def _accumulate_tool_call_deltas(
        cls,
        tc_deltas: list[Any],
        buffers: dict[int, dict[str, Any]],
    ) -> None:
        """把流式 tool_calls 增量块按 index 累积到缓冲区。

        OpenAI/DeepSeek 流式 tool_calls 分多次推送：第一次带 id + function.name，
        后续只带 function.arguments 片段。这里按 index 聚合完整的 id/name/arguments。
        """

        for tc in tc_deltas:
            idx = cls._read_attr_or_key(tc, "index")
            if not isinstance(idx, int):
                idx = 0
            if idx not in buffers:
                buffers[idx] = {
                    "id": "",
                    "function": {"name": "", "arguments": ""},
                }
            buf = buffers[idx]
            tc_id = cls._read_attr_or_key(tc, "id")
            if tc_id:
                buf["id"] = str(tc_id)
            func = cls._read_attr_or_key(tc, "function")
            if isinstance(func, dict):
                fn_name = func.get("name")
                if fn_name:
                    buf["function"]["name"] += str(fn_name)
                fn_args = func.get("arguments")
                if fn_args:
                    buf["function"]["arguments"] += str(fn_args)
            elif func is not None:
                fn_name = getattr(func, "name", None)
                if fn_name:
                    buf["function"]["name"] += str(fn_name)
                fn_args = getattr(func, "arguments", None)
                if fn_args:
                    buf["function"]["arguments"] += str(fn_args)

    def _build_tool_calls_from_deltas(
        self,
        buffers: dict[int, dict[str, Any]],
    ) -> list[ToolCall]:
        """把累积的流式 tool_call 增量块解析为结构化 ToolCall 列表。"""

        calls: list[ToolCall] = []
        for idx in sorted(buffers.keys()):
            buf = buffers[idx]
            fn_name = buf["function"]["name"].strip()
            if not fn_name:
                continue
            calls.append(
                ToolCall(
                    name=self._tool_name_from_function_name(fn_name),
                    arguments=self._parse_tool_arguments(buf["function"]["arguments"]),
                    id=buf["id"],
                    function_name=fn_name,
                )
            )
        return calls

    def _assistant_tool_call_message(
        self,
        raw_message: Any,
        content: str,
        tool_calls: list[ToolCall],
        reasoning: str,
    ) -> dict[str, Any]:
        message: dict[str, Any] = {"role": "assistant", "content": content or None}
        if reasoning:
            message["reasoning_content"] = reasoning
        if tool_calls:
            message["tool_calls"] = [
                {
                    "id": tool_call.id,
                    "type": "function",
                    "function": {
                        "name": tool_call.function_name
                        or self._function_name_for_tool(tool_call.name),
                        "arguments": json.dumps(tool_call.arguments, ensure_ascii=False),
                    },
                }
                for tool_call in tool_calls
            ]
        return message

    def _normalize_tool_call(self, tool_call: ToolCall) -> ToolCall:
        """在执行前归一化模型常见的工具名和参数名误写。"""

        tools = getattr(self, "_tools", {})
        raw_name = re.sub(r"\s+", "", tool_call.name.strip())
        if isinstance(tools, dict) and raw_name not in tools:
            resource_fallback = self._mcp_resource_tool_fallback(raw_name, tools)
            if resource_fallback is not None:
                fallback_name, fallback_path = resource_fallback
                arguments = dict(tool_call.arguments)
                arguments.setdefault("path", fallback_path)
                return ToolCall(
                    name=fallback_name,
                    arguments=self._normalize_tool_arguments(fallback_name, arguments),
                    id=tool_call.id,
                    function_name=tool_call.function_name,
                )

        name = self._normalize_tool_name(tool_call.name)
        return ToolCall(
            name=name,
            arguments=self._normalize_tool_arguments(name, tool_call.arguments),
            id=tool_call.id,
            function_name=tool_call.function_name,
        )

    def _normalize_tool_name(self, raw_name: str) -> str:
        """把 readfile/tablist 这类常见误写映射为当前 Host 真实工具名。"""

        name = re.sub(r"\s+", "", raw_name.strip())
        tools = getattr(self, "_tools", {})
        if isinstance(tools, dict) and name in tools:
            return name

        alias = self._TOOL_NAME_ALIASES.get(name) or self._TOOL_NAME_ALIASES.get(
            self._normalize_identifier(name)
        )
        if alias:
            return alias

        if isinstance(tools, dict) and tools:
            normalized_name = self._normalize_identifier(name)
            matches = [
                tool_name
                for tool_name in tools
                if self._normalize_identifier(tool_name) == normalized_name
            ]
            if len(matches) == 1:
                return matches[0]
        return name

    def _mcp_resource_tool_fallback(
        self,
        requested_name: str,
        tools: dict[str, ToolDefinition],
    ) -> tuple[str, str] | None:
        """兼容模型把项目文档 Resource 工具名写成未注册具体 URI 的情况。

        Local MCP Server 会把实际发现到的 Resource 生成
        `mcp_read_resource__{server}:{uri}` 工具。模型有时会根据文档里的命名规则
        拼出一个当前未注册的项目文档 URI；如果它仍指向工作区内的 Markdown 文档，
        就退回到对应 Server 的 `workspace.read_file`，避免本可读取的文档因工具名
        精确匹配失败而中断。
        """

        prefix = "mcp_read_resource__"
        if not requested_name.startswith(prefix):
            return None

        logical_uri = requested_name[len(prefix) :]
        server_name, separator, resource_uri = logical_uri.partition(":")
        if not separator or not server_name or not resource_uri.startswith("project://"):
            return None

        relative_path = resource_uri[len("project://") :].strip().lstrip("/\\")
        if not relative_path or "\\" in relative_path:
            return None
        path = Path(relative_path)
        if path.is_absolute() or ".." in path.parts or path.suffix.lower() != ".md":
            return None

        fallback_name = f"{server_name}.workspace.read_file"
        if fallback_name not in tools:
            return None
        return fallback_name, relative_path

    def _normalize_tool_arguments(
        self,
        tool_name: str,
        arguments: dict[str, Any],
    ) -> dict[str, Any]:
        """按工具 schema 归一化参数名，兼容 startline/maxlines/tabId 等写法。"""

        canonical_keys = self._tool_argument_keys(tool_name)
        normalized_to_key = {
            self._normalize_identifier(key): key
            for key in canonical_keys
        }
        normalized: dict[str, Any] = {}
        for key, value in arguments.items():
            canonical_key = key
            alias_key = self._ARGUMENT_NAME_ALIASES.get(key) or self._ARGUMENT_NAME_ALIASES.get(
                self._normalize_identifier(key)
            )
            if alias_key in canonical_keys:
                canonical_key = alias_key
            else:
                canonical_key = normalized_to_key.get(self._normalize_identifier(key), key)
            normalized[canonical_key] = value
        return normalized

    def _tool_argument_keys(self, tool_name: str) -> set[str]:
        tools = getattr(self, "_tools", {})
        tool = tools.get(tool_name) if isinstance(tools, dict) else None
        if tool is None:
            return set()

        try:
            schema = json.loads(tool.argument_schema)
        except json.JSONDecodeError:
            return set()
        if not isinstance(schema, dict):
            return set()

        properties = schema.get("properties")
        if isinstance(properties, dict):
            return {key for key in properties if isinstance(key, str)}
        return {key for key in schema if isinstance(key, str)}

    @staticmethod
    def _normalize_identifier(value: str) -> str:
        return re.sub(r"[\s_-]+", "", value).lower()

    def _chat_completion_tools(self) -> list[dict[str, Any]]:
        return [
            {
                "type": "function",
                "function": {
                    "name": self._function_name_for_tool(tool.name),
                    "description": tool.description,
                    "parameters": self._tool_parameters_schema(tool),
                },
            }
            for tool in self._tools.values()
        ]

    def _function_name_for_tool(self, tool_name: str) -> str:
        readable = re.sub(r"[^A-Za-z0-9_]+", "_", tool_name).strip("_").lower()
        readable = readable or "tool"
        digest = hashlib.sha1(tool_name.encode("utf-8")).hexdigest()[:10]
        return f"tool_{readable[:40]}_{digest}"

    def _tool_name_from_function_name(self, function_name: str) -> str:
        for tool_name in getattr(self, "_tools", {}):
            if self._function_name_for_tool(tool_name) == function_name:
                return tool_name
        return function_name

    def _tool_parameters_schema(self, tool: ToolDefinition) -> dict[str, Any]:
        try:
            raw_schema = json.loads(tool.argument_schema)
        except json.JSONDecodeError:
            raw_schema = {}
        if not isinstance(raw_schema, dict):
            raw_schema = {}
        if raw_schema.get("type") == "object" and isinstance(raw_schema.get("properties"), dict):
            schema = dict(raw_schema)
        else:
            properties = {
                key: self._infer_tool_property_schema(value)
                for key, value in raw_schema.items()
                if isinstance(key, str)
            }
            schema = {
                "type": "object",
                "properties": properties,
            }
        schema.setdefault("type", "object")
        schema.setdefault("properties", {})
        return schema

    @staticmethod
    def _infer_tool_property_schema(example: Any) -> dict[str, Any]:
        if isinstance(example, bool):
            return {"type": "boolean"}
        if isinstance(example, int) and not isinstance(example, bool):
            return {"type": "integer"}
        if isinstance(example, (float, int)) and not isinstance(example, bool):
            return {"type": "number"}
        if isinstance(example, list):
            return {"type": "array", "items": {"type": "string"}}
        if isinstance(example, dict):
            return {"type": "object"}
        return {"type": "string"}

    def _run_tool(
        self,
        tool: ToolDefinition,
        arguments: dict[str, Any],
        *,
        on_start: Callable[[], None] | None = None,
    ) -> ToolResult:
        """执行工具；需要审批的工具按当前模式决定是否放行。"""

        if tool.requires_confirmation:
            approved, denial_reason = self._approve_tool_call(tool, arguments)
            if not approved:
                reason = denial_reason or f"未批准执行：{tool.name}。"
                if tool.name in self._mcp_manager.registry.tools:
                    self._mcp_manager.record_denied_tool_call(tool.name, arguments, reason)
                self._append_session_event(
                    "tool_call_denied",
                    {
                        "tool": tool.name,
                        "arguments": arguments,
                        "reason": reason,
                    },
                )
                return ToolResult(ok=False, output=reason)
            self._append_session_event(
                "tool_call_approved",
                {
                    "tool": tool.name,
                    "arguments": arguments,
                    "mode": self.config.approval_mode,
                },
            )

        try:
            if on_start is not None:
                on_start()
            result = tool.run(arguments)
        except Exception as exc:
            return ToolResult(ok=False, output=str(exc))

        return ToolResult(
            ok=result.ok,
            output=self._truncate_tool_output(result.output),
        )

    def _approve_tool_call(
        self,
        tool: ToolDefinition,
        arguments: dict[str, Any],
    ) -> tuple[bool, str]:
        """根据审批模式处理工具许可，返回 (是否批准, 拒绝原因)。"""

        mode = self.config.approval_mode
        if mode == APPROVAL_MODE_AUTO:
            return True, ""
        if mode == APPROVAL_MODE_REVIEW:
            if not self._is_delete_behavior_tool_call(tool, arguments):
                return True, ""
            return self._review_tool_call(tool, arguments)
        return self._confirm(tool.name, arguments), f"用户取消执行：{tool.name}。"

    @classmethod
    def _is_delete_behavior_tool_call(
        cls,
        tool: ToolDefinition,
        arguments: dict[str, Any],
    ) -> bool:
        """判断工具调用是否带有显式删除意图，供 review 模式决定是否进入审查。

        review 模式的目标是减少普通读写、搜索和测试命令的审批噪音，只把真正需要
        守住的删除类动作交给审查模型。这里优先识别工具名和命令字符串，
        同时检查 MCP 常见的 action/operation/method 等意图字段；避免扫描 content
        这类正文参数，以免用户写入的普通文本里出现 delete 一词就被误判。
        """

        if cls._text_has_delete_intent(tool.name):
            return True

        command = arguments.get("command")
        if isinstance(command, str) and cls._command_has_delete_intent(command):
            return True

        if not cls._tool_accepts_shell_command(tool) and cls._description_has_delete_intent(
            tool.description
        ):
            return True

        return cls._arguments_have_delete_intent(arguments, intent_keys=_MCP_DELETE_INTENT_KEYS)

    @staticmethod
    def _tool_accepts_shell_command(tool: ToolDefinition) -> bool:
        return "command" in tool.argument_schema.lower() or "cmd" in tool.argument_schema.lower()

    @classmethod
    def _arguments_have_delete_intent(
        cls,
        value: Any,
        *,
        intent_keys: set[str] = _DELETE_INTENT_KEYS,
    ) -> bool:
        if isinstance(value, dict):
            for raw_key, item in value.items():
                if not isinstance(raw_key, str):
                    continue

                key = raw_key.strip().lower()
                if cls._text_has_delete_intent(key):
                    return True
                if key in intent_keys and isinstance(item, str):
                    if cls._command_has_delete_intent(item) or cls._text_has_delete_intent(item):
                        return True
                elif isinstance(item, dict):
                    if cls._arguments_have_delete_intent(item, intent_keys=intent_keys):
                        return True
                elif isinstance(item, list):
                    if any(
                        cls._arguments_have_delete_intent(child, intent_keys=intent_keys)
                        for child in item
                    ):
                        return True
        elif isinstance(value, list):
            return any(cls._arguments_have_delete_intent(item, intent_keys=intent_keys) for item in value)
        return False

    @staticmethod
    def _command_has_delete_intent(command: str) -> bool:
        return bool(
            _DELETE_COMMAND_PATTERN.search(command)
            or _GIT_CLEAN_PATTERN.search(command)
            or _FIND_DELETE_PATTERN.search(command)
            or _DELETE_INTENT_PATTERN.search(command)
        )

    @staticmethod
    def _text_has_delete_intent(text: str) -> bool:
        if any(term in text for term in _DELETE_LOCALIZED_TERMS):
            return True
        normalized_text = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", text)
        return bool(_DELETE_TEXT_INTENT_PATTERN.search(normalized_text))

    @staticmethod
    def _description_has_delete_intent(text: str) -> bool:
        stripped = text.lstrip(" \t\r\n-_*:;,.")
        if any(stripped.startswith(term) for term in _DELETE_LOCALIZED_TERMS):
            return True
        normalized_text = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", stripped)
        return bool(_DELETE_DESCRIPTION_START_PATTERN.search(normalized_text))

    def _review_tool_call(
        self,
        tool: ToolDefinition,
        arguments: dict[str, Any],
    ) -> tuple[bool, str]:
        """用同一模型的非思考模式审查工具调用是否可自动批准。"""

        review_payload = {
            "tool": tool.name,
            "description": tool.description,
            "arguments": arguments,
            "workspace_root": str(self.workspace_root),
        }
        try:
            response = self._client.responses.create(
                model=self.config.llm.model,
                instructions=TOOL_REVIEW_SYSTEM_PROMPT,
                input=[
                    {
                        "role": "user",
                        "content": json.dumps(review_payload, ensure_ascii=False, indent=2),
                    }
                ],
                extra_body={"thinking": {"type": "disabled"}},
                timeout=min(self.config.request_timeout_seconds, 60),
            )
        except Exception as exc:
            return False, f"自动审查请求失败：{OpenAIResponseLLM.format_request_error(exc)}"

        review_text = OpenAIResponseLLM._extract_text(response)
        approved, reason = self._parse_tool_review_response(review_text)
        if approved:
            return True, ""
        return False, f"自动审查拒绝执行：{reason or '模型未给出批准结论。'}"

    @staticmethod
    def _parse_tool_review_response(review_text: str) -> tuple[bool, str]:
        """解析审查模型 JSON；不可解析时按拒绝处理。"""

        text = review_text.strip()
        if not text:
            return False, "审查模型返回为空。"

        match = re.search(r"\{.*\}", text, re.DOTALL)
        payload = match.group(0) if match else text
        try:
            data = json.loads(payload)
        except json.JSONDecodeError:
            return False, f"审查模型返回不是 JSON：{text}"

        if not isinstance(data, dict):
            return False, "审查模型返回不是 JSON 对象。"

        reason_value = data.get("reason", "")
        reason = reason_value.strip() if isinstance(reason_value, str) else ""
        return data.get("approve") is True, reason

    def _build_tools(self) -> dict[str, ToolDefinition]:
        """注册内置工具；所有工具执行前统一走确认门。"""

        tools = self._build_mcp_tools()
        tools.extend(
            [
                ToolDefinition(
                    name="list_files",
                    description="列出工作区内的文件和目录，可选择递归。",
                    argument_schema='{"path": ".", "recursive": false}',
                    requires_confirmation=True,
                    run=self._tool_list_files,
                ),
                ToolDefinition(
                    name="read_file",
                    description="读取 UTF-8 文本文件，可指定起始行和最多行数。",
                    argument_schema='{"path": "main.py", "start_line": 1, "max_lines": 200}',
                    requires_confirmation=True,
                    run=self._tool_read_file,
                ),
                ToolDefinition(
                    name="search_text",
                    description="在工作区文本文件中搜索正则或普通文本。",
                    argument_schema='{"pattern": "class Agent", "path": ".", "case_sensitive": false, "max_results": 50}',
                    requires_confirmation=True,
                    run=self._tool_search_text,
                ),
                ToolDefinition(
                    name="replace_text",
                    description="在单个文件中替换指定文本，适合小范围代码修改。",
                    argument_schema='{"path": "main.py", "old_text": "...", "new_text": "...", "count": 1}',
                    requires_confirmation=True,
                    run=self._tool_replace_text,
                ),
                ToolDefinition(
                    name="write_file",
                    description=(
                        "写入或追加 UTF-8 文本文件；一次性脚本、中间文件和临时交付物"
                        "应优先写入 Agent 临时目录。"
                    ),
                    argument_schema=(
                        '{"path": ".agent_tmp/files/notes.md", "content": "...", '
                        '"mode": "overwrite"}'
                    ),
                    requires_confirmation=True,
                    run=self._tool_write_file,
                ),
                ToolDefinition(
                    name="run_command",
                    description=(
                        "以工作区为当前目录执行任意本地命令、脚本或 shell 片段。"
                    ),
                    argument_schema='{"command": "python -m py_compile main.py", "timeout_seconds": 120}',
                    requires_confirmation=True,
                    run=self._tool_run_command,
                ),
            ]
        )
        if self._memory_store is not None:
            tools.extend(
                [
                    ToolDefinition(
                        name="memory_search",
                        description="按当前任务检索候选长期记忆摘要，不返回完整正文。",
                        argument_schema=(
                            '{"query":"用户偏好或项目主题","reason":"为什么当前需要查记忆",'
                            '"candidate_directories":["project-context/general"],"max_results":5}'
                        ),
                        requires_confirmation=False,
                        run=self._tool_memory_search,
                    ),
                    ToolDefinition(
                        name="memory_read",
                        description="按记忆 id 读取完整长期记忆内容，并对实际读取的记忆加深回忆。",
                        argument_schema='{"memory_ids":["20260603-164500"]}',
                        requires_confirmation=False,
                        run=self._tool_memory_read,
                    ),
                    ToolDefinition(
                        name="memory_expand_related",
                        description="沿已读记忆的关联目录扩展候选摘要，默认只展开一层关系。",
                        argument_schema='{"memory_ids":["20260603-164500"],"max_depth":1,"max_results":5}',
                        requires_confirmation=False,
                        run=self._tool_memory_expand_related,
                    ),
                    ToolDefinition(
                        name="memory_write",
                        description="写入或合并具有长期价值的记忆，内容应短而准确。",
                        argument_schema=(
                            '{"memories":[{"content":"用户偏好中文交付摘要。",'
                            '"related_directories":["user-preferences/communication-style"],'
                            '"storage_directory":"user-preferences/communication-style",'
                            '"source_event":"本轮对话"}]}'
                        ),
                        requires_confirmation=False,
                        run=self._tool_memory_write,
                    ),
                ]
            )
        return {tool.name: tool for tool in tools}

    def _build_mcp_tools(self) -> list[ToolDefinition]:
        """把 MCP Tool 元数据适配为 Chat Completions function tool。

        Host 内部继续使用 `server.tool` 这类可读名称；对外发送给 DeepSeek 时
        会统一映射成合法 function name，执行时再映射回真实工具名。
        """

        definitions: list[ToolDefinition] = []
        for meta in self._mcp_manager.registry.tools.values():
            definitions.append(
                ToolDefinition(
                    name=meta.logical_name,
                    description=f"{meta.description}（MCP Server：{meta.server_name}）",
                    argument_schema=meta.argument_schema,
                    requires_confirmation=meta.requires_confirmation,
                    run=lambda arguments, tool_meta=meta: self._tool_mcp_call(tool_meta, arguments),
                )
            )
        for meta in self._mcp_manager.registry.resources.values():
            definitions.append(
                ToolDefinition(
                    name=f"mcp_read_resource__{meta.logical_uri}",
                    description=f"读取 MCP Resource：{meta.logical_uri}（MCP Server：{meta.server_name}）",
                    argument_schema='{}',
                    requires_confirmation=False,
                    run=lambda _arguments, logical_uri=meta.logical_uri: self._tool_mcp_read_resource(
                        logical_uri
                    ),
                )
            )
        for meta in self._mcp_manager.registry.prompts.values():
            definitions.append(
                ToolDefinition(
                    name=f"mcp_get_prompt__{meta.logical_name}",
                    description=f"获取 MCP Prompt：{meta.logical_name}（MCP Server：{meta.server_name}）",
                    argument_schema='{"arguments": {}}',
                    requires_confirmation=False,
                    run=lambda arguments, logical_name=meta.logical_name: self._tool_mcp_get_prompt(
                        logical_name,
                        arguments,
                    ),
                )
            )
        return definitions

    def _system_prompt(self) -> str:
        """构造工具协议提示词；每轮强制一个工具或一个最终回答，降低解析复杂度。"""

        tool_lines = "\n".join(
            (
                f"- {tool.name}: {tool.description}\n"
                f"  参数结构：{tool.argument_schema}\n"
            )
            for tool in self._tools.values()
        )
        system_prompt = self._render_system_prompt_template(tool_lines)
        workspace_detection_summary = getattr(
            getattr(self, "config", None),
            "workspace_detection_summary",
            "",
        )
        system_prompt = (
            f"{_runtime_environment_context(self.workspace_root, workspace_detection_summary)}\n\n"
            f"{system_prompt}"
        )
        # 手动调用 /skill:name 时注入 Skill 全文
        if self._skill_manager is not None and self._active_skills:
            system_prompt = self._skill_manager.inject(self._active_skills, system_prompt)
        # 渐进式披露：列出所有可用 Skill 的元数据，AI 自行用 read_file 加载
        elif self._skill_manager is not None:
            metas = self._skill_manager.list_all()
            skill_section = self._skill_manager.format_skills_for_prompt(metas)
            if skill_section:
                system_prompt += f"\n{skill_section}"
        return system_prompt

    def _render_system_prompt_template(self, tool_lines: str) -> str:
        """替换系统提示词模板占位符，同时允许模板中保留 JSON 示例花括号。"""

        temp_workspace = getattr(self, "_temp_workspace", None)
        agent_temp_dir = temp_workspace.display_path if temp_workspace is not None else ".agent_tmp"
        return (
            self._system_prompt_template.replace("{workspace_root}", str(self.workspace_root))
            .replace("{agent_temp_dir}", agent_temp_dir)
            .replace("{tool_lines}", tool_lines)
        )

    def _load_system_prompt_template(self) -> str:
        """读取独立系统提示词模板，避免把长规范硬编码在 Python 代码里。"""

        prompt_path = Path(__file__).resolve().parent / SYSTEM_PROMPT_FILE
        try:
            template = prompt_path.read_text(encoding="utf-8").strip()
        except UnicodeDecodeError as exc:
            raise AgentError(f"{SYSTEM_PROMPT_FILE} 必须是 UTF-8 文本。") from exc
        except OSError as exc:
            raise AgentError(f"读取 {SYSTEM_PROMPT_FILE} 失败：{exc}") from exc

        required_placeholders = ("{workspace_root}", "{agent_temp_dir}", "{tool_lines}")
        missing = [placeholder for placeholder in required_placeholders if placeholder not in template]
        if missing:
            raise AgentError(f"{SYSTEM_PROMPT_FILE} 缺少占位符：{', '.join(missing)}")
        return template

    def _tool_list_files(self, arguments: dict[str, Any]) -> ToolResult:
        try:
            return ToolResult(ok=True, output=self._workspace_toolbox().list_files(arguments))
        except WorkspaceToolError as exc:
            return ToolResult(ok=False, output=str(exc))

    def _tool_read_file(self, arguments: dict[str, Any]) -> ToolResult:
        try:
            return ToolResult(ok=True, output=self._workspace_toolbox().read_file(arguments))
        except WorkspaceToolError as exc:
            return ToolResult(ok=False, output=str(exc))

    def _tool_search_text(self, arguments: dict[str, Any]) -> ToolResult:
        try:
            return ToolResult(ok=True, output=self._workspace_toolbox().search_text(arguments))
        except WorkspaceToolError as exc:
            return ToolResult(ok=False, output=str(exc))

    def _tool_replace_text(self, arguments: dict[str, Any]) -> ToolResult:
        try:
            return ToolResult(ok=True, output=self._workspace_toolbox().replace_text(arguments))
        except WorkspaceToolError as exc:
            return ToolResult(ok=False, output=str(exc))

    def _tool_write_file(self, arguments: dict[str, Any]) -> ToolResult:
        try:
            return ToolResult(ok=True, output=self._workspace_toolbox().write_file(arguments))
        except WorkspaceToolError as exc:
            return ToolResult(ok=False, output=str(exc))

    def _tool_run_command(self, arguments: dict[str, Any]) -> ToolResult:
        try:
            result = self._workspace_toolbox().run_command(arguments)
        except WorkspaceToolError as exc:
            return ToolResult(ok=False, output=str(exc))
        return ToolResult(ok=result.ok, output=result.output)

    def _tool_memory_search(self, arguments: dict[str, Any]) -> ToolResult:
        store = self._require_memory_store()
        query = str(arguments.get("query") or "").strip()
        reason = str(arguments.get("reason") or "").strip()
        if not query:
            return ToolResult(ok=False, output="query 不能为空。")
        if not reason:
            return ToolResult(ok=False, output="reason 不能为空。")

        try:
            results = store.search(
                query=query,
                candidate_directories=self._read_optional_string_list(
                    arguments,
                    "candidate_directories",
                ),
                max_results=self._read_limited_int(arguments, "max_results", default=5, maximum=20),
            )
        except MemoryStoreError as exc:
            return ToolResult(ok=False, output=str(exc))

        return self._json_tool_result([search_result_to_dict(result) for result in results])

    def _tool_memory_read(self, arguments: dict[str, Any]) -> ToolResult:
        store = self._require_memory_store()
        memory_ids = self._read_required_string_list(arguments, "memory_ids")
        if not memory_ids:
            return ToolResult(ok=False, output="memory_ids 不能为空。")

        try:
            records = store.read(memory_ids)
        except MemoryStoreError as exc:
            return ToolResult(ok=False, output=str(exc))

        return self._json_tool_result([record_to_dict(record) for record in records])

    def _tool_memory_expand_related(self, arguments: dict[str, Any]) -> ToolResult:
        store = self._require_memory_store()
        memory_ids = self._read_required_string_list(arguments, "memory_ids")
        if not memory_ids:
            return ToolResult(ok=False, output="memory_ids 不能为空。")

        try:
            results = store.expand_related(
                memory_ids,
                max_depth=self._read_limited_int(arguments, "max_depth", default=1, maximum=3),
                max_results=self._read_limited_int(arguments, "max_results", default=5, maximum=20),
            )
        except MemoryStoreError as exc:
            return ToolResult(ok=False, output=str(exc))

        return self._json_tool_result([search_result_to_dict(result) for result in results])

    def _tool_memory_write(self, arguments: dict[str, Any]) -> ToolResult:
        store = self._require_memory_store()
        raw_memories = arguments.get("memories")
        if not isinstance(raw_memories, list) or not raw_memories:
            return ToolResult(ok=False, output="memories 必须是非空列表。")

        requests: list[MemoryWriteRequest] = []
        for index, raw_memory in enumerate(raw_memories, start=1):
            if not isinstance(raw_memory, dict):
                return ToolResult(ok=False, output=f"第 {index} 条记忆必须是 JSON 对象。")

            content = str(raw_memory.get("content") or "").strip()
            if not content:
                return ToolResult(ok=False, output=f"第 {index} 条记忆 content 不能为空。")

            related = raw_memory.get("related_directories", [])
            if not isinstance(related, list) or not all(isinstance(item, str) for item in related):
                return ToolResult(ok=False, output=f"第 {index} 条记忆 related_directories 必须是字符串列表。")

            storage_directory = raw_memory.get("storage_directory")
            if storage_directory is not None and not isinstance(storage_directory, str):
                return ToolResult(ok=False, output=f"第 {index} 条记忆 storage_directory 必须是字符串或 null。")

            source_event = raw_memory.get("source_event")
            if source_event is not None and not isinstance(source_event, str):
                return ToolResult(ok=False, output=f"第 {index} 条记忆 source_event 必须是字符串或 null。")

            requests.append(
                MemoryWriteRequest(
                    content=content,
                    related_directories=list(related),
                    storage_directory=storage_directory,
                    source_event=source_event,
                )
            )

        try:
            records = store.write(requests)
        except MemoryStoreError as exc:
            return ToolResult(ok=False, output=str(exc))

        return self._json_tool_result([record_to_dict(record) for record in records])

    def _tool_mcp_call(self, meta: MCPToolMeta, arguments: dict[str, Any]) -> ToolResult:
        """执行 MCP Tool，并把 MCP 结构化结果压平为现有 ToolResult。"""

        result = self._mcp_manager.call_tool(meta.logical_name, arguments)
        output_parts = [
            f"MCP Tool：{result.server_name}.{result.tool_name}",
            f"审计 ID：{result.audit_id}",
            f"耗时：{result.duration_ms} ms",
        ]
        if result.error_code:
            output_parts.append(f"错误码：{result.error_code}")
        if result.retryable:
            output_parts.append("可重试：是")
        output_parts.append(f"输出：\n{result.output}")
        return ToolResult(ok=result.ok, output="\n".join(output_parts))

    def _tool_mcp_read_resource(self, logical_uri: str) -> ToolResult:
        """读取 MCP Resource，供模型按需拉取只读上下文。"""

        result = self._mcp_manager.read_resource(logical_uri)
        output_parts = [
            f"MCP Resource：{result.server_name}:{result.uri}",
            f"耗时：{result.duration_ms} ms",
        ]
        if result.error_code:
            output_parts.append(f"错误码：{result.error_code}")
        if result.retryable:
            output_parts.append("可重试：是")
        output_parts.append(f"输出：\n{result.output}")
        return ToolResult(ok=result.ok, output="\n".join(output_parts))

    def _tool_mcp_get_prompt(self, logical_name: str, arguments: dict[str, Any]) -> ToolResult:
        """获取 MCP Prompt 模板，供模型使用稳定任务提示。"""

        raw_arguments = arguments.get("arguments", {})
        if not isinstance(raw_arguments, dict):
            return ToolResult(ok=False, output="arguments 必须是 JSON 对象。")
        result = self._mcp_manager.get_prompt(logical_name, raw_arguments)
        output_parts = [
            f"MCP Prompt：{result.server_name}.{result.prompt_name}",
            f"耗时：{result.duration_ms} ms",
        ]
        if result.error_code:
            output_parts.append(f"错误码：{result.error_code}")
        if result.retryable:
            output_parts.append("可重试：是")
        output_parts.append(f"输出：\n{result.output}")
        return ToolResult(ok=result.ok, output="\n".join(output_parts))

    def _require_memory_store(self) -> MemoryStore:
        if self._memory_store is None:
            raise AgentError("记忆系统未启用。")
        return self._memory_store

    def _require_session_store(self) -> SessionStore:
        if self._session_store is None:
            raise AgentError("会话系统未启用。")
        return self._session_store

    @staticmethod
    def _read_required_string_list(arguments: dict[str, Any], key: str) -> list[str]:
        value = arguments.get(key)
        if not isinstance(value, list):
            return []
        return [item.strip() for item in value if isinstance(item, str) and item.strip()]

    @classmethod
    def _read_optional_string_list(cls, arguments: dict[str, Any], key: str) -> list[str] | None:
        value = arguments.get(key)
        if value is None:
            return None
        if not isinstance(value, list):
            return None
        return cls._read_required_string_list(arguments, key)

    @staticmethod
    def _read_limited_int(
        arguments: dict[str, Any],
        key: str,
        *,
        default: int,
        maximum: int,
    ) -> int:
        value = arguments.get(key, default)
        if isinstance(value, bool):
            return default
        try:
            parsed = int(value)
        except (TypeError, ValueError):
            return default
        return max(1, min(maximum, parsed))

    @staticmethod
    def _json_tool_result(data: Any) -> ToolResult:
        return ToolResult(ok=True, output=json.dumps(data, ensure_ascii=False, indent=2))

    def _workspace_toolbox(self) -> WorkspaceTools:
        toolbox = getattr(self, "_workspace_tools", None)
        if toolbox is not None:
            return toolbox
        command_timeout = getattr(getattr(self, "config", None), "command_timeout_seconds", 120)
        toolbox = WorkspaceTools(
            self.workspace_root,
            command_timeout_seconds=command_timeout,
            extra_protection_message=self._workspace_extra_protection_message,
        )
        self._workspace_tools = toolbox
        return toolbox

    def _workspace_extra_protection_message(self, path: Path) -> str | None:
        """为 Agent 内置工具补充内部目录保护，MCP Server 不共享这条业务限制。"""

        if self._is_memory_path(path):
            return f"请使用 memory_* 工具访问记忆目录：{self._relative_path(path)}"
        if self._is_session_path(path):
            return f"请使用会话命令访问会话目录：{self._relative_path(path)}"
        return None

    def _relative_path(self, path: Path) -> str:
        return self._workspace_toolbox().relative_path(path)

    def _is_memory_path(self, path: Path) -> bool:
        """普通文件工具不直接访问记忆目录，统一走 memory_* 工具。"""

        if self._memory_store is None:
            return False
        try:
            resolved = path.resolve()
        except OSError:
            resolved = path
        return resolved == self._memory_store.root or self._is_relative_to(resolved, self._memory_store.root)

    def _is_session_path(self, path: Path) -> bool:
        """普通文件工具不直接访问会话目录，避免模型误写转录文件。"""

        if getattr(self, "_session_store", None) is None:
            return False
        try:
            resolved = path.resolve()
        except OSError:
            resolved = path
        return resolved == self._session_store.root or self._is_relative_to(resolved, self._session_store.root)

    def _append_session_event(self, event_type: str, payload: dict[str, Any]) -> None:
        """追加会话事件；持久化失败时中断当前任务，避免误以为会话可恢复。"""

        store = getattr(self, "_session_store", None)
        state = getattr(self, "_session_state", None)
        if store is None or state is None:
            return
        try:
            event = store.append_event(state.session_id, event_type, payload)
            self._session_state = SessionState(
                session_id=state.session_id,
                title=state.title,
                workspace_root=state.workspace_root,
                path=state.path,
                created_at=state.created_at,
                updated_at=event.created_at,
                messages=state.messages,
                last_event_type=event.type,
                event_count=state.event_count + 1,
            )
        except SessionStoreError as exc:
            raise AgentError(str(exc)) from exc

    def _append_prompt_history(self, text: str) -> None:
        """记录用户提交的真实提示，用于跨会话输入复用。

        这里和 `user_message` 转录分开写：转录负责恢复模型上下文，提示历史只用于
        UI 的上箭头/搜索复用。持久化失败直接中断本轮，避免用户以为历史已经可恢复。
        """

        store = getattr(self, "_session_store", None)
        state = getattr(self, "_session_state", None)
        if store is None or state is None:
            return
        try:
            store.append_prompt_history(
                display=text,
                workspace_root=self.workspace_root,
                session_id=state.session_id,
            )
        except SessionStoreError as exc:
            raise AgentError(str(exc)) from exc

    def _truncate_tool_output(self, output: str) -> str:
        if len(output) <= self.config.max_tool_output_chars:
            return output
        return output[: self.config.max_tool_output_chars] + "\n... 工具输出已截断。"

    @staticmethod
    def _tool_result_message(tool_call: ToolCall, result: ToolResult) -> dict[str, Any]:
        content = (
            f"状态：{'成功' if result.ok else '失败'}\n"
            f"工具：{tool_call.name}\n"
            f"结果：\n{result.output}"
        )
        return {
            "role": "tool",
            "tool_call_id": tool_call.id or tool_call.name,
            "content": content,
        }

    @staticmethod
    def _assistant_message(assistant_text: str, reasoning: str = "") -> dict[str, Any]:
        return {"role": "assistant", "content": assistant_text}

    def _append_history(self, user_text: str, assistant_text: str, reasoning: str = "") -> None:
        """写入对话历史；reasoning 仅用于本地兼容签名，不回传给 Chat Completions。"""

        self._history.extend(
            [
                {"role": "user", "content": user_text},
                self._assistant_message(assistant_text, reasoning),
            ]
        )
        max_messages = self.config.max_history_turns * 2
        if len(self._history) > max_messages:
            self._history = self._history[-max_messages:]

    @staticmethod
    def _confirm_in_terminal(tool_name: str, arguments: dict[str, Any]) -> bool:
        print("\nAgent 请求执行受限工具：")
        print(f"工具：{tool_name}")
        print("参数：")
        print(json.dumps(arguments, ensure_ascii=False, indent=2))
        answer = input("是否允许执行？直接回车=YES，输入 n/no/否=NO：").strip().lower()
        return answer not in {"n", "no", "否", "false"}

    @staticmethod
    def _is_relative_to(path: Path, parent: Path) -> bool:
        try:
            path.relative_to(parent)
            return True
        except ValueError:
            return False
