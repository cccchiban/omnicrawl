"""结构化 git 工具的 ToolResult 适配层。

通过独立的 ``git`` 子进程直接执行（不经 shell），模型提供的参数以 argv
列表逐项传递，从根源上消除 shell 转义与命令注入；操作目标固定为工作区，
``paths`` 强制限制在工作区内，并拒绝 ``--git-dir`` / ``--work-tree`` /
``--no-verify`` 等逃逸类参数。

审批风险分级（readonly / local / high）由 :mod:`omnicrawl.agent.toolkit.approval_policy`
集中定义；本模块只负责参数校验、argv 构建、执行与有界输出。
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Any

from .approval_policy import GIT_SUPPORTED_ACTIONS
from .tools import read_required_string_list
from ..types import ToolResult

GIT_COMMAND_TIMEOUT_SECONDS = 360
# git 输出有界化：超长时保留首尾并提示截断（避免大 diff/日志淹没模型上下文）。
GIT_OUTPUT_HEAD_CHARS = 4_000
GIT_OUTPUT_TAIL_CHARS = 8_000
GIT_OUTPUT_MAX_CHARS = GIT_OUTPUT_HEAD_CHARS + GIT_OUTPUT_TAIL_CHARS

# 逃逸类参数：这些参数会把 git 操作目标指向工作区之外或绕过仓库校验，一律拒绝。
_FORBIDDEN_ARGUMENT_TOKENS = (
    "--git-dir",
    "--work-tree",
    "--no-verify",
    "--no-commit-verify",
)
_FORBIDDEN_ARGUMENT_PREFIXES = ("--git-dir=", "--work-tree=")
# config 作用域参数：--global/--system/--file 会写入工作区之外（用户/系统配置
# 或任意文件），结构化工具只允许修改当前仓库的本地配置。
_CONFIG_FORBIDDEN_TOKENS = ("--global", "--system", "--file")
_CONFIG_FORBIDDEN_PREFIXES = ("--file=",)
# archive 的 -o/--output 会把归档写到任意路径；只允许输出到 stdout（有界采样）。
_ARCHIVE_FORBIDDEN_TOKENS = ("-o", "--output")
_ARCHIVE_FORBIDDEN_PREFIXES = ("--output=",)
# clone 目标目录 / init 目录必须位于工作区内。
_DIRECTORY_ACTIONS = frozenset({"clone", "init"})


def git_result(workspace_root: Path, arguments: dict[str, Any]) -> ToolResult:
    """校验参数、构建 git argv 并执行，返回有界 ToolResult。"""

    action = str(arguments.get("action") or "").strip().casefold()
    if action not in GIT_SUPPORTED_ACTIONS:
        return _error_result(f"不支持的 git 子命令：{action or '(空)'}。")

    args = read_required_string_list(arguments, "args")
    message = arguments.get("message")
    if message is not None and not isinstance(message, str):
        return _error_result("message 必须是字符串。")
    message = message.strip() if isinstance(message, str) else ""
    paths = read_required_string_list(arguments, "paths")

    forbidden = _forbidden_token(args)
    if forbidden:
        return _error_result(f"git 工具不允许参数：{forbidden}。")
    if action == "config" and (
        any(token in args for token in _CONFIG_FORBIDDEN_TOKENS)
        or any(arg.startswith(prefix) for prefix in _CONFIG_FORBIDDEN_PREFIXES for arg in args)
    ):
        return _error_result("git config 不允许修改全局/系统配置或指定 --file。")
    if action == "archive" and (
        any(token in args for token in _ARCHIVE_FORBIDDEN_TOKENS)
        or any(arg.startswith(prefix) for prefix in _ARCHIVE_FORBIDDEN_PREFIXES for arg in args)
    ):
        return _error_result("git archive 不允许 -o/--output，输出只能走 stdout。")
    if action == "commit" and not message and "--no-edit" not in args:
        return _error_result("commit 必须提供 message，或显式传入 --no-edit。")

    path_error = _validate_workspace_paths(workspace_root, paths)
    if path_error:
        return _error_result(path_error)
    if action in _DIRECTORY_ACTIONS:
        directory_error = _validate_directory_positional(workspace_root, action, args)
        if directory_error:
            return _error_result(directory_error)

    # argv[0] 必须是 git 本身：Windows 下 subprocess 把第一个参数当可执行文件。
    argv = ["git", "-c", "color.ui=never", "--no-pager", action, *args]
    if message and action in {"commit", "tag"}:
        argv.extend(["-m", message])
    if paths:
        argv.append("--")
        argv.extend(paths)

    try:
        completed = subprocess.run(
            argv,
            cwd=str(workspace_root),
            env=_git_environment(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=GIT_COMMAND_TIMEOUT_SECONDS,
        )
    except FileNotFoundError:
        return _error_result("未找到 git 可执行文件，请确认 Git 已安装。")
    except subprocess.TimeoutExpired:
        return _error_result(f"git 命令超时（>{GIT_COMMAND_TIMEOUT_SECONDS}s）：{action}。")
    except OSError as exc:
        return _error_result(f"无法启动 git：{exc}。")

    stdout = completed.stdout.decode("utf-8", errors="replace")
    stderr = completed.stderr.decode("utf-8", errors="replace")
    if completed.returncode != 0:
        return ToolResult(
            ok=False,
            output=_bound_text(
                stderr.strip() or f"git {action} 失败（退出码 {completed.returncode}）。"
            ),
        )
    return ToolResult(ok=True, output=_bound_text(stdout))


def _git_environment() -> dict[str, str]:
    env = dict(os.environ)
    env["GIT_PAGER"] = "cat"
    env["PAGER"] = "cat"
    # 不向终端交互：需要凭据时直接失败而不是挂起等待输入；
    # 需要编辑器时用 true 直接成功，避免 commit --amend 等阻塞。
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GIT_EDITOR"] = "true"
    return env


def _forbidden_token(args: list[str]) -> str:
    for arg in args:
        if arg in _FORBIDDEN_ARGUMENT_TOKENS or arg.startswith(_FORBIDDEN_ARGUMENT_PREFIXES):
            return arg
    return ""


def _validate_workspace_paths(workspace_root: Path, paths: list[str]) -> str:
    if not paths:
        return ""
    workspace = Path(workspace_root).resolve()
    for path in paths:
        try:
            candidate = (workspace / path).resolve()
        except OSError:
            return f"路径无法解析（必须在工作区内）：{path}。"
        if candidate != workspace and workspace not in candidate.parents:
            return f"路径越界（必须在工作区内）：{path}。"
    return ""


def _validate_directory_positional(
    workspace_root: Path,
    action: str,
    args: list[str],
) -> str:
    """校验 clone/init 的目标目录位于工作区内（clone 的目标是最后一个位置参数）。"""

    positionals = [arg for arg in args if not arg.startswith("-")]
    if not positionals:
        return ""
    directory = positionals[-1]
    workspace = Path(workspace_root).resolve()
    try:
        candidate = (workspace / directory).resolve()
    except OSError:
        return f"{action} 目标目录无法解析（必须在工作区内）：{directory}。"
    if candidate != workspace and workspace not in candidate.parents:
        return f"{action} 目标目录越界（必须在工作区内）：{directory}。"
    return ""


def _bound_text(text: str) -> str:
    if len(text) <= GIT_OUTPUT_MAX_CHARS:
        return text
    head = text[:GIT_OUTPUT_HEAD_CHARS]
    tail = text[-GIT_OUTPUT_TAIL_CHARS:]
    return (
        f"{head}\n…（git 输出已截断，共 {len(text)} 字符）\n{tail}"
    )


def _error_result(message: str) -> ToolResult:
    return ToolResult(ok=False, output=message)
