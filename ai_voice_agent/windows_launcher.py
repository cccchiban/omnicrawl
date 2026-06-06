from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


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
    workdir = str(script_path.parent)
    command = (
        f"$env:{POWERSHELL_CHILD_ENV}='1'; "
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; "
        f"& {sys.executable!r} {str(script_path)!r}; "
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
            cwd=workdir,
            creationflags=subprocess.CREATE_NEW_CONSOLE,
        )
    except OSError as exc:
        print(f"弹出 PowerShell 窗口失败，将在当前窗口继续运行：{exc}")
        return False

    print("已弹出独立 PowerShell 窗口，请在新窗口中进行语音对话。")
    return True
