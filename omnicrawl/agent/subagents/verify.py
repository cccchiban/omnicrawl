"""`verify` SubAgent 的固定检查工具。

该模块刻意不接收任意 Shell 文本，也不把命令列表放到用户配置中。模型只能选择
Host 维护的检查标识，随后由本模块把它解析为固定 argv 并交给
:class:`WorkspaceTools` 以 ``shell=False`` 启动。这样 verify profile 能运行必要的
本地回归检查，同时不会成为 Bash、PowerShell、文件写入或联网执行的权限旁路。
"""

from __future__ import annotations

import json
import sys
from dataclasses import dataclass
from typing import Any

from ..types import ToolDefinition, ToolResult
from ...workspace.tools import WorkspaceToolError, WorkspaceTools


VERIFY_COMMAND_TOOL_NAME = "verify_command"
_ALLOWED_ARGUMENTS = frozenset({"check", "timeout_seconds"})


@dataclass(frozen=True)
class VerifyCheck:
    """一项由 Host 固定维护的、无模型可控 argv 的验证检查。"""

    identifier: str
    label: str
    argv: tuple[str, ...]


# 注意：不要把用户输入、Agent prompt、配置字符串或工作区相对路径拼接到这些 argv。
# 首期只开放无删除、无写文件、无网络与无变更性 Git 操作的检查；需要更多检查时，
# 应新增一个明确的静态条目并补充对应的安全/回归测试，而不是开放原始 command 参数。
VERIFY_CHECKS: dict[str, VerifyCheck] = {
    "unit_tests": VerifyCheck(
        identifier="unit_tests",
        label="Python 单元测试",
        argv=(sys.executable, "-m", "unittest", "discover", "-s", "tests", "-q"),
    ),
    "compileall": VerifyCheck(
        identifier="compileall",
        label="Python 编译检查",
        argv=(sys.executable, "-m", "compileall", "-q", "omnicrawl", "main.py", "tests"),
    ),
    "git_diff_check": VerifyCheck(
        identifier="git_diff_check",
        label="Git 差异空白检查",
        argv=("git", "diff", "--check"),
    ),
}


def build_verify_command_tool(
    workspace_tools: WorkspaceTools,
    *,
    max_timeout_seconds: int,
) -> ToolDefinition:
    """创建只供 verify profile 注入的受控检查工具。

    这个工具不会加入父 Agent 的普通工具表，避免主对话获得无关的新入口；
    Coordinator 仅在 ``explicit-command-allowlist`` profile 且配置显式启用时注入它。
    """

    if isinstance(max_timeout_seconds, bool) or not isinstance(max_timeout_seconds, int):
        raise ValueError("verify 命令超时必须是整数。")
    if max_timeout_seconds < 1:
        raise ValueError("verify 命令超时必须大于 0。")

    argument_schema = json.dumps(
        {
            "type": "object",
            "properties": {
                "check": {
                    "type": "string",
                    "enum": list(VERIFY_CHECKS),
                    "description": "Host 固定验证检查的标识，不接受命令文本。",
                },
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": max_timeout_seconds,
                    "description": "可选的更短超时，不能超过 Host 配置上限。",
                },
            },
            "required": ["check"],
            "additionalProperties": False,
        },
        ensure_ascii=False,
    )
    return ToolDefinition(
        name=VERIFY_COMMAND_TOOL_NAME,
        description=(
            "执行 Host 固定的本地验证检查。仅支持 unit_tests、compileall、"
            "git_diff_check；不能传递 Bash、PowerShell、命令文本、路径、环境变量或网络参数。"
        ),
        argument_schema=argument_schema,
        requires_confirmation=False,
        run=lambda arguments: run_verify_command(
            workspace_tools,
            arguments,
            max_timeout_seconds=max_timeout_seconds,
        ),
    )


def run_verify_command(
    workspace_tools: WorkspaceTools,
    arguments: dict[str, Any],
    *,
    max_timeout_seconds: int,
) -> ToolResult:
    """校验模型参数，解析静态 argv，并执行一项固定检查。"""

    try:
        check, timeout_seconds = _parse_verify_arguments(
            arguments,
            max_timeout_seconds=max_timeout_seconds,
        )
        result = workspace_tools.run_argv_command(
            check.argv,
            timeout_seconds=timeout_seconds,
            label=check.label,
        )
    except WorkspaceToolError as exc:
        return ToolResult(ok=False, output=str(exc))
    return ToolResult(ok=result.ok, output=result.output)


def _parse_verify_arguments(
    arguments: dict[str, Any],
    *,
    max_timeout_seconds: int,
) -> tuple[VerifyCheck, int]:
    """只接受检查标识和不超过 Host 上限的整数超时。"""

    if not isinstance(arguments, dict):
        raise WorkspaceToolError("verify_command 参数必须是对象。")
    unsupported = sorted(set(arguments) - _ALLOWED_ARGUMENTS)
    if unsupported:
        raise WorkspaceToolError(
            f"verify_command 不支持参数：{'、'.join(unsupported)}。"
        )

    check_id = arguments.get("check")
    if not isinstance(check_id, str) or check_id not in VERIFY_CHECKS:
        available = "、".join(VERIFY_CHECKS)
        raise WorkspaceToolError(f"check 必须是固定检查标识：{available}。")

    value = arguments.get("timeout_seconds", max_timeout_seconds)
    if isinstance(value, bool) or not isinstance(value, int):
        raise WorkspaceToolError("timeout_seconds 必须是整数。")
    if value < 1 or value > max_timeout_seconds:
        raise WorkspaceToolError(
            f"timeout_seconds 必须在 1 到 {max_timeout_seconds} 之间。"
        )
    return VERIFY_CHECKS[check_id], value


__all__ = [
    "VERIFY_COMMAND_TOOL_NAME",
    "VERIFY_CHECKS",
    "VerifyCheck",
    "build_verify_command_tool",
    "run_verify_command",
]
