from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from .project_context import LAUNCH_CWD_ENV

POWERSHELL_CHILD_ENV = "AI_VOICE_CHAT_IN_POWERSHELL"


def configure_console_encoding() -> None:
    """尽量使用 UTF-8 输出，减少 Windows 命令行中文乱码概率。"""

    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")


def _running_in_powershell_child() -> bool:
    """判断当前进程是否已经是弹出窗口中的真实对话进程。"""

    return os.getenv(POWERSHELL_CHILD_ENV) == "1"


def launch_in_powershell_window(script_path: Path) -> bool:
    """从 IDE 或测试窗口启动时，弹出独立 PowerShell 运行本脚本。"""

    if os.name != "nt" or _running_in_powershell_child():
        return False

    script_path = script_path.resolve()
    launch_cwd = Path.cwd().resolve()
    command = (
        f"$env:{POWERSHELL_CHILD_ENV}='1'; "
        f"$env:{LAUNCH_CWD_ENV}={_powershell_single_quoted(str(launch_cwd))}; "
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; "
        f"& {_powershell_single_quoted(sys.executable)} {_powershell_single_quoted(str(script_path))}; "
        "Write-Host ''; "
        "Read-Host '对话已结束，按 Enter 关闭窗口'"
    )

    try:
        subprocess.Popen(
            [
                "powershell.exe",
                "-NoExit",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ],
            cwd=str(launch_cwd),
            creationflags=subprocess.CREATE_NEW_CONSOLE,
        )
    except OSError as exc:
        print(f"弹出 PowerShell 窗口失败，将在当前窗口继续运行：{exc}")
        return False

    print("已弹出独立 PowerShell 窗口，请在新窗口中进行语音对话。")
    return True


def _powershell_single_quoted(value: str) -> str:
    """生成 PowerShell 单引号字符串，避免路径中的空格或特殊字符破坏启动命令。"""

    return "'" + value.replace("'", "''") + "'"
