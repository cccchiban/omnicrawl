"""Read-only command policy for delegated SubAgents.

The policy deliberately allows a small, auditable command surface instead of
trying to infer arbitrary shell side effects. Browser actions and remote MCP
side effects are outside the local-workspace write boundary; direct shell file
mutation is rejected before the underlying command runner is called.
"""

from __future__ import annotations

import re
import shlex
from dataclasses import replace
from typing import Any

from ..approval_policy import (
    GIT_TIER_READONLY,
    command_has_git_mutation_intent,
    git_action_tier,
)
from ..types import ToolDefinition, ToolResult


READ_ONLY_COMMAND_TOOL_NAMES = frozenset({"bash", "powershell", "monitor"})

_BASH_COMMANDS = frozenset(
    {
        "agent-browser-cli",
        "basename",
        "cat",
        "curl",
        "date",
        "dirname",
        "du",
        "echo",
        "file",
        "find",
        "git",
        "grep",
        "head",
        "jq",
        "ls",
        "nl",
        "printf",
        "pwd",
        "rg",
        "sed",
        "sort",
        "stat",
        "tail",
        "tr",
        "tree",
        "uname",
        "uniq",
        "wc",
        "which",
        "whoami",
    }
)
_POWERSHELL_COMMANDS = frozenset(
    {
        "agent-browser-cli",
        "curl",
        "curl.exe",
        "format-list",
        "format-table",
        "get-childitem",
        "get-command",
        "get-content",
        "get-date",
        "get-item",
        "get-location",
        "get-process",
        "get-service",
        "git",
        "invoke-restmethod",
        "invoke-webrequest",
        "measure-object",
        "resolve-path",
        "select-object",
        "select-string",
        "sort-object",
        "test-path",
        "where.exe",
    }
)
_BROWSER_COMMANDS = frozenset(
    {
        "click",
        "close",
        "console",
        "doctor",
        "exec",
        "fill",
        "logs",
        "lookup",
        "mouse-click",
        "network",
        "open",
        "profile-label",
        "restart",
        "save-pdf",
        "scan",
        "screenshot",
        "send-keys",
        "snapshot",
        "status",
        "stop",
        "tabs",
        "tabtree",
    }
)
_CURL_FILE_FLAGS = frozenset(
    {
        "--cookie-jar",
        "--dump-header",
        "--output",
        "--output-dir",
        "--remote-header-name",
        "--remote-name",
        "--trace",
        "--trace-ascii",
    }
)
_CURL_REMOTE_WRITE_FLAGS = frozenset(
    {
        "--data",
        "--data-ascii",
        "--data-binary",
        "--data-raw",
        "--form",
        "--form-string",
        "--json",
        "--upload-file",
    }
)
_CURL_REMOTE_WRITE_METHODS = frozenset({"delete", "patch", "post", "put"})
_FIND_WRITE_ACTIONS = frozenset(
    {
        "-delete",
        "-exec",
        "-execdir",
        "-fls",
        "-fprint",
        "-fprintf",
        "-ok",
        "-okdir",
    }
)


def wrap_read_only_command_tool(tool: ToolDefinition) -> ToolDefinition:
    """Wrap a shell/monitor tool with a pre-execution read-only guard."""

    if tool.name not in READ_ONLY_COMMAND_TOOL_NAMES:
        return tool

    def guarded(arguments: dict[str, Any]) -> ToolResult:
        reason = read_only_command_denial_reason(tool.name, arguments)
        if reason:
            return ToolResult(ok=False, output=f"[read-only] 命令已拒绝：{reason}")
        return tool.run(arguments)

    return replace(tool, requires_confirmation=False, run=guarded)


def wrap_read_only_git_tool(tool: ToolDefinition) -> ToolDefinition:
    """Wrap the structured git tool so only read-only git actions can run.

    ``git_action_tier`` 是审批侧的分级规则：只读档（diff/log/show/status 等）
    直接放行；本地变更与高风险档（add/commit/push/reset --hard 等）一律拒绝，
    确保只读子代理用结构化 git 工具收集 diff 时不可能改写工作区。
    """

    if tool.name != "git":
        return tool

    def guarded(arguments: dict[str, Any]) -> ToolResult:
        if not isinstance(arguments, dict):
            return ToolResult(ok=False, output="[read-only] git 参数必须是对象。")
        tier = git_action_tier(arguments)
        if tier != GIT_TIER_READONLY:
            action = str(arguments.get("action") or "未知")
            return ToolResult(ok=False, output=f"[read-only] git {action} 不属于只读操作。")
        return tool.run(arguments)

    return replace(tool, requires_confirmation=False, run=guarded)


def read_only_command_denial_reason(
    tool_name: str,
    arguments: dict[str, Any],
) -> str:
    """Return an empty string when a command call is provably read-only."""

    if tool_name == "monitor":
        action = str(arguments.get("action") or "").strip().casefold()
        if action in {"list", "get", "poll", "stop"}:
            return ""
        if action != "start":
            return "monitor 只允许 start/list/get/poll/stop。"
        shell = str(arguments.get("shell") or "powershell").strip().casefold()
    elif tool_name in {"bash", "powershell"}:
        shell = tool_name
    else:
        return f"不支持的命令工具：{tool_name}。"

    command = arguments.get("command")
    if not isinstance(command, str) or not command.strip():
        return "缺少非空 command。"
    reason = _command_text_denial_reason(command, shell=shell)
    if reason:
        return reason

    diagnostic = arguments.get("diagnostic_command")
    if diagnostic is not None:
        if not isinstance(diagnostic, str):
            return "diagnostic_command 必须是字符串。"
        if not diagnostic.strip():
            return ""
        reason = _command_text_denial_reason(diagnostic, shell=shell)
        if reason:
            return f"diagnostic_command 不符合只读策略：{reason}"
    return ""


def _command_text_denial_reason(command: str, *, shell: str) -> str:
    if len(command) > 8_000:
        return "命令超过 8000 字符，无法可靠审查。"
    if "`" in command or "$(``" in command or "$(" in command:
        return "不允许命令替换或反引号执行。"
    if shell == "powershell" and any(char in command for char in "{}"):
        return "PowerShell 脚本块无法证明只读。"

    segments, error = _split_shell_segments(command)
    if error:
        return error
    for segment in segments:
        reason = _segment_denial_reason(segment, shell=shell)
        if reason:
            return reason
    return ""


def _split_shell_segments(command: str) -> tuple[list[str], str]:
    """Split simple command chains while rejecting local file redirection."""

    segments: list[str] = []
    current: list[str] = []
    quote = ""
    escaped = False
    index = 0
    while index < len(command):
        char = command[index]
        if escaped:
            current.append(char)
            escaped = False
            index += 1
            continue
        if char == "\\" and quote != "'":
            current.append(char)
            escaped = True
            index += 1
            continue
        if quote:
            current.append(char)
            if char == quote:
                quote = ""
            index += 1
            continue
        if char in {"'", '"'}:
            quote = char
            current.append(char)
            index += 1
            continue
        if char in {">", "<"}:
            return [], "不允许输入或输出重定向。"
        if char in {";", "\n", "|", "&"}:
            segment = "".join(current).strip()
            if not segment:
                return [], "命令链包含空片段。"
            segments.append(segment)
            current = []
            if index + 1 < len(command) and command[index + 1] == char:
                index += 1
            index += 1
            continue
        current.append(char)
        index += 1

    if quote:
        return [], "命令包含未闭合引号。"
    segment = "".join(current).strip()
    if not segment:
        return [], "命令为空。"
    segments.append(segment)
    return segments, ""


def _segment_denial_reason(segment: str, *, shell: str) -> str:
    try:
        tokens = shlex.split(segment, posix=shell != "powershell")
    except ValueError:
        return "命令参数无法可靠解析。"
    if not tokens:
        return "命令片段为空。"

    executable = _normalized_executable(tokens[0])
    allowed = _POWERSHELL_COMMANDS if shell == "powershell" else _BASH_COMMANDS
    if executable not in allowed:
        return f"命令 {tokens[0]} 不在只读允许列表中。"

    lowered = [token.casefold() for token in tokens[1:]]
    if executable == "git" and command_has_git_mutation_intent(segment):
        return "Git 子命令会修改工作树、索引、引用、配置或远端。"
    if executable in {"curl", "curl.exe"}:
        reason = _curl_denial_reason(tokens[1:])
        if reason:
            return reason
    if executable == "sed" and any(
        token == "--in-place" or re.fullmatch(r"-i.*", token)
        for token in lowered
    ):
        return "sed --in-place/-i 会修改文件。"
    if executable == "find" and any(
        token in _FIND_WRITE_ACTIONS for token in lowered
    ):
        return "find 的写入、删除或 exec 动作不允许。"
    if executable == "agent-browser-cli":
        return _browser_cli_denial_reason(tokens[1:])
    if executable in {"invoke-webrequest", "invoke-restmethod"}:
        if any(token in {"-outfile", "-out-file"} for token in lowered):
            return "PowerShell Web 请求的 -OutFile 会写入本地文件。"
        for index, token in enumerate(lowered):
            if token == "-method" and index + 1 < len(lowered):
                if lowered[index + 1] in _CURL_REMOTE_WRITE_METHODS:
                    return "PowerShell Web 请求方法可能修改远端状态。"
    return ""


def _curl_denial_reason(arguments: list[str]) -> str:
    for index, token in enumerate(arguments):
        lowered = token.casefold()
        option = lowered.split("=", 1)[0]
        if option in _CURL_REMOTE_WRITE_FLAGS or token.startswith(("-d", "-F", "-T")):
            return f"curl {option} 可能修改远端状态。"
        if option == "--request" or token.startswith("-X"):
            method = (
                lowered.split("=", 1)[1]
                if "=" in lowered
                else (
                    token[2:].casefold()
                    if token.startswith("-X") and len(token) > 2
                    else (
                        arguments[index + 1].casefold()
                        if index + 1 < len(arguments)
                        else ""
                    )
                )
            )
            if method in _CURL_REMOTE_WRITE_METHODS:
                return f"curl {method.upper()} 可能修改远端状态。"
        if option in _CURL_FILE_FLAGS:
            return f"curl {option} 会写入本地文件。"
        if token.startswith("-") and not token.startswith("--"):
            flags = token[1:]
            if any(flag in flags for flag in ("o", "O", "D", "c")):
                return f"curl {token} 可能写入本地文件。"
    return ""


def _browser_cli_denial_reason(arguments: list[str]) -> str:
    if not arguments:
        return "agent-browser-cli 缺少子命令。"
    subcommand = arguments[0].casefold()
    if subcommand.startswith("-"):
        return "" if subcommand in {"--help", "--version"} else "未知浏览器 CLI 选项。"
    if subcommand not in _BROWSER_COMMANDS:
        return f"agent-browser-cli {arguments[0]} 不在运行期允许列表中。"
    for index, token in enumerate(arguments):
        if token == "--out" or token.startswith("--out="):
            return "显式 --out 可能覆盖工作区文件；请使用 CLI 默认临时目录。"
        if token == "--file" and index + 1 >= len(arguments):
            return "--file 缺少输入路径。"
    return ""


def _normalized_executable(token: str) -> str:
    normalized = token.strip().strip('"\'').replace("\\", "/").rsplit("/", 1)[-1]
    lowered = normalized.casefold()
    for suffix in (".exe", ".cmd", ".bat"):
        if lowered.endswith(suffix) and lowered not in {"curl.exe", "where.exe"}:
            return lowered[: -len(suffix)]
    return lowered


__all__ = [
    "READ_ONLY_COMMAND_TOOL_NAMES",
    "read_only_command_denial_reason",
    "wrap_read_only_command_tool",
    "wrap_read_only_git_tool",
]
