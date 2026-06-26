from __future__ import annotations

import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .agent_types import ToolResult
from .workspace_tools import DEFAULT_COMMAND_TIMEOUT_SECONDS, MAX_COMMAND_TIMEOUT_SECONDS


DEFAULT_BB_BROWSER_TIMEOUT_SECONDS = DEFAULT_COMMAND_TIMEOUT_SECONDS
MAX_BB_BROWSER_TIMEOUT_SECONDS = MAX_COMMAND_TIMEOUT_SECONDS
MAX_BB_BROWSER_ARGS = 80
MAX_BB_BROWSER_ARG_CHARS = 20_000


class BBBrowserCLIError(RuntimeError):
    """bb-browser CLI 参数校验、启动或执行失败。"""


@dataclass(frozen=True)
class BBBrowserCLI:
    """Host 侧 bb-browser CLI 能力。

    bb-browser 自身已经把 CLI 作为主入口：普通命令会先检查 daemon 状态，
    必要时自动启动 daemon，并在没有可用 CDP 端口时拉起一个受管浏览器。
    因此 Agent 只需要把结构化参数转成进程参数列表，不再经由 MCP 转发。
    """

    workspace_root: Path

    def run(self, arguments: dict[str, Any]) -> ToolResult:
        try:
            args = self._read_args(arguments)
            output = self._run_bb_browser(args, self._read_timeout(arguments))
        except BBBrowserCLIError as exc:
            return ToolResult(ok=False, output=str(exc))
        return ToolResult(ok=True, output=output)

    def ensure_started(self) -> tuple[bool, str]:
        """显式预热 bb-browser daemon。

        普通 Agent 启动不会调用这里，避免用户未请求浏览器能力时弹出受管
        浏览器。只有明确需要提前检查浏览器环境的调用方才应使用这个方法；
        日常浏览器任务直接走 run()，由 bb-browser CLI 在首次真实命令时按需
        处理 daemon、CDP 发现和受管浏览器启动。
        """

        try:
            output = self._run_bb_browser(
                ["daemon", "start", "--json"],
                DEFAULT_BB_BROWSER_TIMEOUT_SECONDS,
            )
        except BBBrowserCLIError as exc:
            return False, str(exc)
        return True, output

    def _run_bb_browser(self, args: list[str], timeout: int) -> str:
        command = [*_resolve_bb_browser_command(self.workspace_root), *args]
        try:
            completed = subprocess.run(
                command,
                cwd=str(self.workspace_root),
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=timeout,
            )
        except FileNotFoundError as exc:
            raise BBBrowserCLIError(
                "找不到 bb-browser CLI。请先执行 npm install，或设置 BB_BROWSER_COMMAND。"
            ) from exc
        except subprocess.TimeoutExpired as exc:
            raise BBBrowserCLIError(f"bb-browser 命令超过 {timeout} 秒，已终止。") from exc
        except OSError as exc:
            raise BBBrowserCLIError(f"bb-browser 命令执行失败：{exc}") from exc

        stdout = completed.stdout.strip()
        stderr = completed.stderr.strip()
        if completed.returncode != 0:
            parts = [f"bb-browser 退出码：{completed.returncode}"]
            if stdout:
                parts.append(f"stdout:\n{stdout}")
            if stderr:
                parts.append(f"stderr:\n{stderr}")
            raise BBBrowserCLIError("\n\n".join(parts))
        return stdout or stderr or "bb-browser 命令已完成。"

    @staticmethod
    def _read_args(arguments: dict[str, Any]) -> list[str]:
        raw_args = arguments.get("args", [])
        if not isinstance(raw_args, list) or not all(isinstance(item, str) for item in raw_args):
            raise BBBrowserCLIError("args 必须是字符串数组。")
        if not raw_args:
            raise BBBrowserCLIError("args 不能为空，例如 ['status', '--json']。")
        if len(raw_args) > MAX_BB_BROWSER_ARGS:
            raise BBBrowserCLIError(f"args 最多 {MAX_BB_BROWSER_ARGS} 项。")

        args: list[str] = []
        for item in raw_args:
            if not item:
                raise BBBrowserCLIError("args 不能包含空字符串。")
            if len(item) > MAX_BB_BROWSER_ARG_CHARS:
                raise BBBrowserCLIError(
                    f"单个参数不能超过 {MAX_BB_BROWSER_ARG_CHARS} 字符。"
                )
            args.append(item)
        return args

    @staticmethod
    def _read_timeout(arguments: dict[str, Any]) -> int:
        raw_value = arguments.get("timeout_seconds", DEFAULT_BB_BROWSER_TIMEOUT_SECONDS)
        if isinstance(raw_value, bool):
            return DEFAULT_BB_BROWSER_TIMEOUT_SECONDS
        try:
            value = int(raw_value)
        except (TypeError, ValueError):
            return DEFAULT_BB_BROWSER_TIMEOUT_SECONDS
        return max(1, min(value, MAX_BB_BROWSER_TIMEOUT_SECONDS))


def _resolve_bb_browser_command(workspace_root: Path) -> list[str]:
    raw_command = os.getenv("BB_BROWSER_COMMAND", "").strip()
    if raw_command:
        return [raw_command]

    local_bin = workspace_root / "node_modules" / ".bin" / (
        "bb-browser.cmd" if os.name == "nt" else "bb-browser"
    )
    if local_bin.is_file():
        return [str(local_bin)]

    resolved = shutil.which("bb-browser")
    if resolved:
        return [resolved]

    npx = shutil.which("npx")
    if npx:
        return [npx, "-y", "bb-browser"]

    return ["bb-browser"]
