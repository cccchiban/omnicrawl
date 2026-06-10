from __future__ import annotations

import json
import os
import platform
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from .approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_REVIEW,
    load_approval_mode,
    normalize_approval_mode,
)
from .llm import LLMConfig, OpenAIResponseLLM, VALID_REASONING_EFFORTS, load_llm_config
from .memory import (
    MemoryStore,
    MemoryStoreError,
    MemoryWriteRequest,
    record_to_dict,
    search_result_to_dict,
)
from .mcp import MCPClientManager, MCPConfig, MCPConfigError, MCPToolMeta, load_mcp_config
from .skill import SkillManager, SkillMatchResult


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


def _runtime_environment_context(workspace_root: Path) -> str:
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


class AgentError(RuntimeError):
    """Agent 循环、工具调用或安全校验失败时抛出。"""


class _EmptyAgentReply(RuntimeError):
    """网关请求成功但没有返回可用文本，交由上层按策略重试。"""


@dataclass(frozen=True)
class ToolCall:
    """模型请求执行的一次工具调用。"""

    name: str
    arguments: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class ToolResult:
    """工具调用返回给模型的结构化结果。"""

    ok: bool
    output: str


@dataclass(frozen=True)
class ToolDefinition:
    """Agent 可用工具的说明与执行函数。"""

    name: str
    description: str
    argument_schema: str
    requires_confirmation: bool
    run: Callable[[dict[str, Any]], ToolResult]


class _AgentReplyStreamer:
    """流式输出最终回复，同时避免工具调用前言打乱对话顺序。"""

    _OPEN_TAG = "<final>"
    _CLOSE_TAG = "</final>"
    _TOOL_TAG = "<tool>"
    _PROTOCOL_TAG_STARTS = ("<tool", "<final")
    _PLAIN_FINAL_LOOKAHEAD_CHARS = 10

    def __init__(self, on_delta: Callable[[str], None]) -> None:
        self._on_delta = on_delta
        self._prefix_buffer = ""
        self._tail_buffer = ""
        self._inside_final = False
        self._inside_plain_final = False
        self._closed = False
        self.streamed = False

    def push(self, delta: str) -> None:
        if self._closed or not delta:
            return

        if self._inside_plain_final:
            # 即使已进入普通文本模式，后续增量仍可能包含 <tool> 或 <final>
            # 标签。例如模型先输出"好的，开始安装。"再输出 <tool>...</tool>。
            # 保留尾部少量字符用于跨 delta 边界的标签检测，避免工具 JSON
            # 被当成普通文本直接展示在终端。
            combined = self._prefix_buffer + delta
            lowered = combined.lower()
            tag_pos = self._first_protocol_tag_index(lowered)
            if tag_pos is not None:
                # 找到标签起始位置，只发送标签前的安全文本。
                if tag_pos > 0:
                    self._emit(combined[:tag_pos])
                self._closed = True
                self._prefix_buffer = ""
                return
            keep = min(len(combined), len(self._TOOL_TAG))
            if len(combined) > keep:
                self._emit(combined[:-keep])
                self._prefix_buffer = combined[-keep:]
            else:
                self._prefix_buffer = combined
            return

        if not self._inside_final:
            self._prefix_buffer += delta
            stripped = self._prefix_buffer.lstrip()
            lowered = stripped.lower()
            if not stripped:
                return

            if lowered.startswith(self._OPEN_TAG):
                self._inside_final = True
                self._push_final_text(stripped[len(self._OPEN_TAG) :])
                self._prefix_buffer = ""
                return

            if self._is_protocol_tag_prefix(lowered):
                return

            first_char = stripped[0]
            if first_char in {"<", "{"} or self._TOOL_TAG.rstrip(">") in lowered:
                self._closed = True
                self._prefix_buffer = ""
                return

            # 普通文本可能只是工具调用前的说明，例如“好的，开始安装。”后面紧跟
            # <tool>。因此先留一个很短的观察窗口；一旦内容已经明显是自然语言
            # 最终回答，就提前放行，避免没有 <final> 标签时整段回复等到结尾才显示。
            if self._looks_like_plain_final(stripped, lowered):
                self._inside_plain_final = True
                self._prefix_buffer = ""
                self._emit(stripped)
            return

        self._push_final_text(delta)

    @classmethod
    def _looks_like_plain_final(cls, stripped: str, lowered: str) -> bool:
        if not stripped or cls._is_protocol_tag_prefix(lowered):
            return False
        if cls._first_protocol_tag_index(lowered) is not None:
            return False
        if re.match(r"^\s{0,3}(#{1,6}\s+|[-*+]\s+|\d+[.)]\s+|>\s+)", stripped):
            return True
        return len(stripped) >= cls._PLAIN_FINAL_LOOKAHEAD_CHARS

    @classmethod
    def _is_protocol_tag_prefix(cls, lowered: str) -> bool:
        """当前文本是否可能是协议标签的前缀（含完整标签本身）。

        完整标签也返回 True，因为模型可能先输出无参数的 <tool> 再增量输出 JSON；
        此时应继续缓冲，避免把标签裸文本刷到终端。
        """

        return any(tag.startswith(lowered) for tag in (cls._OPEN_TAG, cls._TOOL_TAG))

    @classmethod
    def _first_protocol_tag_index(cls, lowered: str) -> int | None:
        """返回文本中最早出现的工具或最终回答标签位置。"""

        positions = [
            position
            for tag_start in cls._PROTOCOL_TAG_STARTS
            if (position := lowered.find(tag_start)) >= 0
        ]
        return min(positions) if positions else None

    def finish(self) -> None:
        if self._inside_final and not self._closed and self._tail_buffer:
            self._emit(self._tail_buffer)
            self._tail_buffer = ""
        if self._inside_plain_final and not self._closed and self._prefix_buffer:
            self._emit(self._prefix_buffer)
            self._prefix_buffer = ""

    def _push_final_text(self, text: str) -> None:
        self._tail_buffer += text
        close_index = self._tail_buffer.lower().find(self._CLOSE_TAG)
        if close_index >= 0:
            self._emit(self._tail_buffer[:close_index])
            self._tail_buffer = ""
            self._closed = True
            return

        keep_chars = len(self._CLOSE_TAG) - 1
        if len(self._tail_buffer) <= keep_chars:
            return

        self._emit(self._tail_buffer[:-keep_chars])
        self._tail_buffer = self._tail_buffer[-keep_chars:]

    def _emit(self, text: str) -> None:
        if not text:
            return
        self.streamed = True
        self._on_delta(text)


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
    mcp_config: MCPConfig | None = None
    approval_mode: str = field(default_factory=load_approval_mode)
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
        self.approval_mode = normalize_approval_mode(self.approval_mode)


class LocalToolAgent:
    """能在本地项目内读文件、检索、按确认执行写入/命令的简化 Agent Harness。

    参考 pi 的核心思想：Agent 不是一次问答，而是"模型 -> 工具 -> 观察 -> 下一轮模型"的循环。
    当前实现选择文本协议而不是原生 Responses tool call，是为了兼容课程网关可能只实现
    OpenAI Responses 的文本流部分这一现实约束。
    """

    _TOOL_OPEN_PATTERN = re.compile(r"^\s*(?:\^\s*)?<tool\b[^>]*>\s*", re.IGNORECASE)
    _FINAL_PATTERN = re.compile(r"<final>\s*(.*?)\s*</final>", re.DOTALL | re.IGNORECASE)
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
        self._memory_store = self._create_memory_store() if self.config.memory_enabled else None
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
        """开启新对话：清空对话历史，保留工具、记忆和 Skill 配置。"""

        self._history.clear()
        self._active_skills = []

    def close(self) -> None:
        """关闭 Agent 持有的外部资源，当前主要是 MCP stdio 子进程。"""

        manager = getattr(self, "_mcp_manager", None)
        if manager is not None:
            manager.close()

    @property
    def approval_mode(self) -> str:
        """当前工具审批模式，供 TUI 展示和斜杠命令切换。"""

        return self.config.approval_mode

    def set_approval_mode(self, mode: str) -> None:
        """运行时切换审批模式；持久化由调用方负责写入 config.json。"""

        self.config.approval_mode = normalize_approval_mode(mode)

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
        on_token_usage: Callable[[int, int], None] | None = None,
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
        report_token_usage = on_token_usage or (lambda _input_tokens, _output_tokens: None)

        text = self._apply_skill_command(text, status)
        working_messages = [
            *self._project_instructions_messages(),
            *self._history,
            {"role": "user", "content": text},
        ]

        all_reasoning_parts: list[str] = []
        has_tool_calls = False
        step = 1
        while True:
            raw_reply, reasoning, streamed_final = self._request_agent_reply(
                working_messages,
                on_delta,
                report_token_usage,
            )
            if reasoning:
                all_reasoning_parts.append(reasoning)
            tool_call = self._parse_tool_call(raw_reply)
            if tool_call is None:
                final_reply = self._parse_final_reply(raw_reply)
                if not streamed_final:
                    on_delta(final_reply)
                combined_reasoning = "\n".join(all_reasoning_parts) if has_tool_calls else ""
                self._append_history(text, final_reply, combined_reasoning)
                return final_reply

            tool_call = self._normalize_tool_call(tool_call)
            has_tool_calls = True
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
            status("")  # 通知调用方重新启动等待动画

            working_messages.extend(
                [
                    self._assistant_message(raw_reply, reasoning),
                    {
                        "role": "user",
                        "content": self._format_tool_observation(tool_call, tool_result),
                    },
                ]
            )
            step += 1

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

    def _project_instructions_messages(self) -> list[dict[str, str]]:
        """构造每次请求最前方的项目规范上下文消息。

        这个消息不写入 `_history`，但会在每次发起模型请求时放在 input 列表开头。
        对无服务端会话状态的 Responses 调用来说，模型只能看到本次请求携带的
        input；因此项目规范必须随每次请求发送一次，但不能累积进本地历史，否则
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
        messages: list[dict[str, str]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int], None],
    ) -> tuple[str, str, bool]:
        """请求模型给出下一步：要么调用一个工具，要么输出最终回答。

        返回 (reply, reasoning, streamed_final)。工具调用场景中 reasoning 会被
        保留在 working_messages 里并持续回传 API。
        """

        last_empty_reply: _EmptyAgentReply | None = None
        for attempt in range(1, self.config.request_retry_count + 1):
            try:
                return self._request_agent_reply_once(messages, on_delta, on_token_usage)
            except _EmptyAgentReply as exc:
                last_empty_reply = exc
                if attempt >= self.config.request_retry_count:
                    break

        raise AgentError(
            f"Agent 连续 {self.config.request_retry_count} 次返回空响应，已停止本轮请求。"
        ) from last_empty_reply

    def _request_agent_reply_once(
        self,
        messages: list[dict[str, str]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int], None],
    ) -> tuple[str, str, bool]:
        """执行一次模型流式请求；空响应由调用方统一重试。"""

        try:
            stream = self._client.responses.create(
                model=self.config.llm.model,
                instructions=self._system_prompt(),
                input=messages,
                stream=True,
                extra_body=self._build_extra_body(),
                timeout=self.config.request_timeout_seconds,
            )
        except Exception as exc:
            raise AgentError(f"Agent 请求失败：{OpenAIResponseLLM.format_request_error(exc)}") from exc

        chunks: list[str] = []
        reasoning_chunks: list[str] = []
        latest_token_usage: tuple[int, int] | None = None
        final_streamer = _AgentReplyStreamer(on_delta)
        try:
            for event in stream:
                event_usage = OpenAIResponseLLM.extract_token_usage(event)
                if event_usage is not None:
                    latest_token_usage = event_usage
                for delta in OpenAIResponseLLM.extract_stream_text(event):
                    chunks.append(delta)
                    final_streamer.push(delta)
                if self.config.llm.thinking_enabled:
                    for delta in OpenAIResponseLLM.extract_stream_reasoning(event):
                        reasoning_chunks.append(delta)
        except Exception as exc:
            raise AgentError(f"Agent 流式回复中断：{OpenAIResponseLLM.format_request_error(exc)}") from exc

        final_streamer.finish()
        reply = "".join(chunks)
        if not reply.strip():
            raise _EmptyAgentReply("Agent 返回内容为空或格式不可解析。")

        reasoning = "".join(reasoning_chunks).strip()
        if latest_token_usage is not None:
            on_token_usage(*latest_token_usage)
        return reply.strip(), reasoning, final_streamer.streamed

    def _build_extra_body(self) -> dict[str, Any]:
        """构造网关扩展参数；根据 reasoning_effort 决定是否启用思考模式。"""

        thinking_type = "enabled" if self.config.llm.thinking_enabled else "disabled"
        body: dict[str, Any] = {"thinking": {"type": thinking_type}}
        if self.config.llm.thinking_enabled and self.config.llm.reasoning_effort:
            if self.config.llm.reasoning_effort in VALID_REASONING_EFFORTS:
                body["reasoning_effort"] = self.config.llm.reasoning_effort
        return body

    def _parse_tool_call(self, raw_reply: str) -> ToolCall | None:
        """从模型回复中解析工具调用；解析失败时退化为最终回答，避免卡死。"""

        payload = self._extract_tool_payload(raw_reply)
        if payload is None:
            return None

        try:
            data = json.loads(payload)
        except json.JSONDecodeError:
            return None

        if not isinstance(data, dict):
            return None

        name = data.get("name") or data.get("tool")
        arguments = data.get("arguments") or data.get("args") or {}
        if not isinstance(name, str) or not isinstance(arguments, dict):
            return None

        return ToolCall(name=self._normalize_tool_name(name), arguments=arguments)

    @classmethod
    def _extract_tool_payload(cls, raw_reply: str) -> str | None:
        """提取工具调用 JSON。

        标准协议要求 `<tool>{...}</tool>`，但实际模型可能把闭合标签漏掉，
        或在第一个工具 JSON 后继续追加第二个 `<tool>`。这里只接受回复起始处
        的工具标签或原始 JSON，并用 JSONDecoder 提取第一个完整对象，避免把
        普通回答中间的 `<tool>` 误当成真实工具调用。
        """

        stripped = raw_reply.strip()
        if stripped.startswith("{"):
            return cls._extract_first_json_payload(stripped)

        open_match = cls._TOOL_OPEN_PATTERN.match(stripped)
        if not open_match:
            return None
        payload = stripped[open_match.end() :].strip()
        if not payload:
            return None
        return cls._extract_first_json_payload(payload)

    @classmethod
    def _extract_first_json_payload(cls, text: str) -> str | None:
        """返回文本开头第一个 JSON 对象，容忍常见的协议拼接错误。

        模型偶尔会输出 `<tool>{"name":...,"arguments":{...}</tool>`，也就是
        `arguments` 对象闭合了，但最外层工具对象少了一个 `}`。这类错误如果
        直接退化成最终回答，会把裸 `<tool>` 标签刷到 TUI。这里仅在文本位于
        协议边界前、且只缺少 JSON 对象/数组闭合符时补齐，其他语法错误仍然
        返回 None，避免把普通文本误当成可执行工具。
        """

        text = text.strip()
        if not text:
            return None

        decoded_payload = cls._try_extract_json_prefix(text)
        if decoded_payload is not None:
            return decoded_payload

        boundary_index = cls._first_tool_payload_boundary_index(text)
        candidate = text[:boundary_index].strip() if boundary_index is not None else text
        return cls._try_complete_json_object(candidate)

    @staticmethod
    def _try_extract_json_prefix(text: str) -> str | None:
        try:
            decoded, end_index = json.JSONDecoder().raw_decode(text)
        except json.JSONDecodeError:
            return None
        if not isinstance(decoded, dict):
            return None
        return text[:end_index].strip()

    @staticmethod
    def _first_tool_payload_boundary_index(text: str) -> int | None:
        lowered = text.lower()
        positions = [
            position
            for marker in ("</tool>", "<tool", "<final")
            if (position := lowered.find(marker)) >= 0
        ]
        return min(positions) if positions else None

    @classmethod
    def _try_complete_json_object(cls, text: str) -> str | None:
        text = text.strip()
        if not text.startswith("{"):
            return None

        stack: list[str] = []
        in_string = False
        escaped = False
        for char in text:
            if in_string:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    in_string = False
                continue

            if char == '"':
                in_string = True
            elif char in "{[":
                stack.append(char)
            elif char in "}]":
                if not stack:
                    return None
                opening = stack.pop()
                if (opening, char) not in {("{", "}"), ("[", "]")}:
                    return None

        if in_string or escaped or not stack:
            return None

        suffix = "".join("}" if opening == "{" else "]" for opening in reversed(stack))
        return cls._try_extract_json_prefix(f"{text}{suffix}")

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
                )

        name = self._normalize_tool_name(tool_call.name)
        return ToolCall(
            name=name,
            arguments=self._normalize_tool_arguments(name, tool_call.arguments),
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

    def _parse_final_reply(self, raw_reply: str) -> str:
        """提取最终回答；没有显式 final 标签时直接使用模型原文。"""

        match = self._FINAL_PATTERN.search(raw_reply)
        if match:
            return match.group(1).strip()
        return raw_reply.strip()

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
                return ToolResult(ok=False, output=reason)

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
                    description="写入或追加 UTF-8 文本文件。",
                    argument_schema='{"path": "notes.md", "content": "...", "mode": "overwrite"}',
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
        """把 MCP Tool 元数据适配为现有文本协议工具。

        这里不改变模型侧协议，只把 MCP Tool 以 `server.tool` 名称追加到工具列表。
        审批仍复用 `_run_tool` 的统一入口，具体是否需要确认由 MCP Host 策略决定。
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
                f"  参数示例：{tool.argument_schema}\n"
            )
            for tool in self._tools.values()
        )
        system_prompt = self._render_system_prompt_template(tool_lines)
        system_prompt = f"{_runtime_environment_context(self.workspace_root)}\n\n{system_prompt}"
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

        return (
            self._system_prompt_template.replace("{workspace_root}", str(self.workspace_root))
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

        required_placeholders = ("{workspace_root}", "{tool_lines}")
        missing = [placeholder for placeholder in required_placeholders if placeholder not in template]
        if missing:
            raise AgentError(f"{SYSTEM_PROMPT_FILE} 缺少占位符：{', '.join(missing)}")
        return template

    def _tool_list_files(self, arguments: dict[str, Any]) -> ToolResult:
        path = self._safe_path(str(arguments.get("path") or "."))
        recursive = bool(arguments.get("recursive", False))
        if not path.exists():
            return ToolResult(ok=False, output=f"路径不存在：{self._relative_path(path)}")
        if path.is_file():
            return ToolResult(ok=True, output=self._relative_path(path))

        entries: list[str] = []
        iterator = path.rglob("*") if recursive else path.iterdir()
        for entry in sorted(iterator, key=lambda item: str(item).lower()):
            if self._should_skip_path(entry):
                continue
            suffix = "/" if entry.is_dir() else ""
            entries.append(f"{self._relative_path(entry)}{suffix}")
            if len(entries) >= 500:
                entries.append("... 已截断，结果超过 500 项。")
                break

        return ToolResult(ok=True, output="\n".join(entries) or "目录为空。")

    def _tool_read_file(self, arguments: dict[str, Any]) -> ToolResult:
        path = self._safe_path(str(arguments.get("path") or ""))
        start_line = max(1, int(arguments.get("start_line") or 1))
        max_lines = max(1, min(500, int(arguments.get("max_lines") or 200)))
        if not path.is_file():
            return ToolResult(ok=False, output=f"不是文件：{self._relative_path(path)}")

        text = self._read_text(path)
        lines = text.splitlines()
        start_index = start_line - 1
        selected = lines[start_index : start_index + max_lines]
        numbered = [f"{line_no}: {line}" for line_no, line in enumerate(selected, start=start_line)]
        if start_index + max_lines < len(lines):
            numbered.append("... 已截断，可提高 start_line 继续读取。")
        return ToolResult(ok=True, output="\n".join(numbered))

    def _tool_search_text(self, arguments: dict[str, Any]) -> ToolResult:
        pattern = str(arguments.get("pattern") or "")
        if not pattern:
            return ToolResult(ok=False, output="pattern 不能为空。")

        root = self._safe_path(str(arguments.get("path") or "."))
        case_sensitive = bool(arguments.get("case_sensitive", False))
        max_results = max(1, min(200, int(arguments.get("max_results") or 50)))
        flags = 0 if case_sensitive else re.IGNORECASE

        try:
            regex = re.compile(pattern, flags)
        except re.error:
            regex = re.compile(re.escape(pattern), flags)

        files = [root] if root.is_file() else self._iter_search_files(root)
        results: list[str] = []
        for file_path in files:
            if self._should_skip_path(file_path):
                continue
            try:
                lines = self._read_text(file_path).splitlines()
            except AgentError:
                continue
            for line_no, line in enumerate(lines, start=1):
                if regex.search(line):
                    results.append(f"{self._relative_path(file_path)}:{line_no}: {line}")
                    if len(results) >= max_results:
                        return ToolResult(ok=True, output="\n".join(results) + "\n... 已达到 max_results。")

        return ToolResult(ok=True, output="\n".join(results) or "未找到匹配结果。")

    def _tool_replace_text(self, arguments: dict[str, Any]) -> ToolResult:
        path = self._safe_path(str(arguments.get("path") or ""))
        old_text = str(arguments.get("old_text") or "")
        new_text = str(arguments.get("new_text") or "")
        count = int(arguments.get("count") or 1)
        if not path.is_file():
            return ToolResult(ok=False, output=f"不是文件：{self._relative_path(path)}")
        if not old_text:
            return ToolResult(ok=False, output="old_text 不能为空。")

        original = self._read_text(path)
        occurrences = original.count(old_text)
        if occurrences == 0:
            return ToolResult(ok=False, output="未找到 old_text，文件未修改。")

        replace_count = occurrences if count <= 0 else min(count, occurrences)
        updated = original.replace(old_text, new_text, replace_count)
        path.write_text(updated, encoding="utf-8")
        return ToolResult(
            ok=True,
            output=f"已修改 {self._relative_path(path)}，替换 {replace_count} 处。",
        )

    def _tool_write_file(self, arguments: dict[str, Any]) -> ToolResult:
        path = self._safe_path(str(arguments.get("path") or ""))
        content = str(arguments.get("content") or "")
        mode = str(arguments.get("mode") or "overwrite").lower()
        path.parent.mkdir(parents=True, exist_ok=True)
        if mode == "append":
            with path.open("a", encoding="utf-8") as file:
                file.write(content)
            action = "追加"
        elif mode in {"overwrite", "write"}:
            path.write_text(content, encoding="utf-8")
            action = "写入"
        else:
            return ToolResult(ok=False, output="mode 仅支持 overwrite 或 append。")

        return ToolResult(ok=True, output=f"已{action} {self._relative_path(path)}，字符数：{len(content)}。")

    def _tool_run_command(self, arguments: dict[str, Any]) -> ToolResult:
        command = str(arguments.get("command") or "").strip()
        if not command:
            return ToolResult(ok=False, output="command 不能为空。")

        timeout = int(arguments.get("timeout_seconds") or self.config.command_timeout_seconds)
        timeout = max(1, min(timeout, 300))
        try:
            completed = subprocess.run(
                command,
                cwd=str(self.workspace_root),
                shell=True,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=timeout,
            )
        except subprocess.TimeoutExpired:
            return ToolResult(ok=False, output=f"命令执行超过 {timeout} 秒，已终止。")

        output_parts = [f"退出码：{completed.returncode}"]
        if completed.stdout.strip():
            output_parts.append(f"stdout:\n{completed.stdout.strip()}")
        if completed.stderr.strip():
            output_parts.append(f"stderr:\n{completed.stderr.strip()}")
        return ToolResult(ok=completed.returncode == 0, output="\n\n".join(output_parts))

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

    def _iter_search_files(self, root: Path) -> list[Path]:
        """递归搜索时在目录层剪枝，避免进入 .git、虚拟环境或本地密钥目录。"""

        files: list[Path] = []
        for dirpath, dirnames, filenames in os.walk(root):
            current_dir = Path(dirpath)
            dirnames[:] = [
                dirname
                for dirname in sorted(dirnames, key=lambda s: s.lower())
                if not self._should_skip_path(current_dir / dirname)
            ]

            for filename in sorted(filenames, key=lambda s: s.lower()):
                file_path = current_dir / filename
                if not self._should_skip_path(file_path):
                    files.append(file_path)

        return files

    def _require_memory_store(self) -> MemoryStore:
        if self._memory_store is None:
            raise AgentError("记忆系统未启用。")
        return self._memory_store

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

    def _safe_path(self, raw_path: str) -> Path:
        """把模型给出的路径限制在工作区内，阻止 ../ 越界访问。"""

        raw_path = raw_path.strip()
        if not raw_path:
            raise AgentError("路径不能为空。")

        candidate = Path(raw_path)
        if not candidate.is_absolute():
            candidate = self.workspace_root / candidate
        resolved = candidate.resolve()
        if not self._is_relative_to(resolved, self.workspace_root):
            raise AgentError(f"拒绝访问工作区外路径：{raw_path}")
        if self._is_protected_path(resolved):
            raise AgentError(f"拒绝访问受保护路径：{self._relative_path(resolved)}")
        if self._is_memory_path(resolved):
            raise AgentError(f"请使用 memory_* 工具访问记忆目录：{self._relative_path(resolved)}")
        return resolved

    def _relative_path(self, path: Path) -> str:
        try:
            return str(path.resolve().relative_to(self.workspace_root))
        except ValueError:
            return str(path)

    def _should_skip_path(self, path: Path) -> bool:
        return self._is_protected_path(path) or self._is_memory_path(path)

    @staticmethod
    def _is_protected_path(path: Path) -> bool:
        """避免自动工具读取缓存、虚拟环境、Git 内部文件或本地密钥文件。"""

        protected_names = {
            ".git",
            ".venv",
            "venv",
            "env",
            "__pycache__",
            ".codex-ref",
            ".env",
            "config.json",
        }
        return any(part in protected_names or part.startswith(".env.") for part in path.parts)

    def _is_memory_path(self, path: Path) -> bool:
        """普通文件工具不直接访问记忆目录，统一走 memory_* 工具。"""

        if self._memory_store is None:
            return False
        try:
            resolved = path.resolve()
        except OSError:
            resolved = path
        return resolved == self._memory_store.root or self._is_relative_to(resolved, self._memory_store.root)

    def _read_text(self, path: Path) -> str:
        try:
            return path.read_text(encoding="utf-8")
        except UnicodeDecodeError as exc:
            raise AgentError(f"文件不是 UTF-8 文本或包含二进制内容：{self._relative_path(path)}") from exc
        except OSError as exc:
            raise AgentError(f"读取文件失败：{self._relative_path(path)}，{exc}") from exc

    def _truncate_tool_output(self, output: str) -> str:
        if len(output) <= self.config.max_tool_output_chars:
            return output
        return output[: self.config.max_tool_output_chars] + "\n... 工具输出已截断。"

    @staticmethod
    def _format_tool_observation(tool_call: ToolCall, result: ToolResult) -> str:
        return (
            f"工具 {tool_call.name} 执行完成。\n"
            f"状态：{'成功' if result.ok else '失败'}\n"
            f"结果：\n{result.output}\n\n"
            "请基于该工具结果继续。若任务已经完成，请用 <final> 输出最终回答；"
            "若还需要更多信息，请继续用 <tool> 调用一个工具。"
        )

    @staticmethod
    def _assistant_message(assistant_text: str, reasoning: str = "") -> dict[str, str]:
        message = {"role": "assistant", "content": assistant_text}
        if reasoning:
            message["reasoning_content"] = reasoning
        return message

    def _append_history(self, user_text: str, assistant_text: str, reasoning: str = "") -> None:
        """写入对话历史；有工具调用时附带 reasoning_content 供后续轮次回传。"""

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
