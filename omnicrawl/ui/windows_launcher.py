
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from ..workspace.context import LAUNCH_CWD_ENV

POWERSHELL_CHILD_ENV = "AI_VOICE_CHAT_IN_POWERSHELL"


def _powershell_terminal_cleanup() -> str:
    """生成由父 PowerShell 执行的终端协议复位命令。"""

    # Python 子进程可能在 Textual Driver 完成清理前异常结束，因此最终兜底必须
    # 由仍然存活的父 PowerShell 执行。Windows PowerShell 5.1 不支持 `\e` 转义，
    # 使用 [char]27 兼容系统自带版本；短暂等待后清空已经排队的 VT 输入，
    # 避免鼠标移动序列被后续 Read-Host 当成普通文字回显。
    return (
        "$esc=[char]27; "
        "[Console]::Write("
        '"${esc}[?1000l${esc}[?1002l${esc}[?1003l${esc}[?1015l${esc}[?1006l'
        '${esc}[?1004l${esc}[?2004l${esc}[<u${esc}[?1049l${esc}[?25h"); '
        "[Console]::Out.Flush(); "
        "Start-Sleep -Milliseconds 50; "
        "try { $Host.UI.RawUI.FlushInputBuffer() } catch {}; "
    )


def configure_console_encoding() -> None:
    """尽量使用 UTF-8 输出，减少 Windows 命令行中文乱码概率。"""

    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")


def _running_in_powershell_child() -> bool:
    """判断当前进程是否已经是弹出窗口中的真实对话进程。"""

    return os.getenv(POWERSHELL_CHILD_ENV) == "1"


def launch_in_powershell_window(script_path: Path, argv: list[str] | None = None) -> bool:
    """从 IDE 或测试窗口启动时，弹出独立 PowerShell 运行本脚本。"""

    if os.name != "nt" or _running_in_powershell_child():
        return False

    script_path = script_path.resolve()
    launch_cwd = Path.cwd().resolve()
    args = list(sys.argv[1:] if argv is None else argv)
    script_args = "".join(f" {_powershell_single_quoted(argument)}" for argument in args)
    command = (
        f"$env:{POWERSHELL_CHILD_ENV}='1'; "
        f"$env:{LAUNCH_CWD_ENV}={_powershell_single_quoted(str(launch_cwd))}; "
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; "
        f"& {_powershell_single_quoted(sys.executable)} {_powershell_single_quoted(str(script_path))}{script_args}; "
        "$appExitCode=$LASTEXITCODE; "
        f"{_powershell_terminal_cleanup()}"
        "Write-Host ''; "
        "if ($appExitCode -ne 0) { "
        "Write-Host \"OmniCrawl 界面意外退出（代码 $appExitCode），请保留上方错误信息。\" "
        "-ForegroundColor Red; "
        "}; "
        "Read-Host '对话已结束，按 Enter 关闭窗口'"
        "; exit $appExitCode"
    )

    try:
        subprocess.Popen(
            [
                "powershell.exe",
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
