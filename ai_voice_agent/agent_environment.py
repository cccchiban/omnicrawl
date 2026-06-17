from __future__ import annotations

import os
import platform
import sys
from pathlib import Path


def runtime_environment_context(
    workspace_root: Path,
    workspace_detection_summary: str = "",
    *,
    window_hint: str = "",
    command_shell_hint: str = "",
    terminal_hint: str = "",
) -> str:
    """生成注入给模型的运行环境摘要。

    调用方可显式传入检测结果，便于测试和兼容旧补丁点；未传入时本模块自行检测。
    这里只暴露低敏、稳定且会影响工具选择的信息；不枚举完整环境变量，
    避免把 API Key、Token、代理配置等敏感值塞进模型上下文。
    """

    detected_window_hint = window_hint or detect_agent_window_hint()
    detected_command_shell_hint = command_shell_hint or detect_command_shell_hint()
    detected_terminal_hint = terminal_hint or detect_terminal_hint()
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
    if detected_window_hint:
        lines.append(f"- Agent 运行窗口：{detected_window_hint}")
    if detected_command_shell_hint:
        lines.append(f"- run_command 默认 Shell：{detected_command_shell_hint}")
    if detected_terminal_hint:
        lines.append(f"- 终端环境变量：{detected_terminal_hint}")
    return "\n".join(lines)


def detect_command_shell_hint() -> str:
    """检测 run_command 使用 shell=True 时最应遵循的命令语法。"""

    if os.name == "nt":
        comspec = os.getenv("COMSPEC", "").strip()
        shell = comspec or "cmd.exe"
        return f"{shell}（默认按 CMD 语法解析；PowerShell 语法需显式调用 powershell.exe -Command）"
    return os.getenv("SHELL", "").strip()


def detect_agent_window_hint() -> str:
    """检测 Agent 所在的交互窗口或父进程链，帮助模型选择兼容命令。"""

    if os.name != "nt":
        shell = os.getenv("SHELL", "").strip()
        terminal = detect_terminal_hint()
        if shell and terminal:
            return f"Shell={Path(shell).name}；终端={terminal}"
        return f"Shell={Path(shell).name}" if shell else terminal

    process_chain = windows_process_name_chain()
    lowered_chain = [name.lower() for name in process_chain]
    shell_label = windows_shell_label(lowered_chain)
    terminal_label = windows_terminal_label(lowered_chain)

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


def windows_shell_label(lowered_process_chain: list[str]) -> str:
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


def windows_terminal_label(lowered_process_chain: list[str]) -> str:
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


def windows_process_name_chain(limit: int = 12) -> list[str]:
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


def detect_terminal_hint() -> str:
    """返回终端类型线索，只使用常见非敏感变量名。"""

    hints: list[str] = []
    for name in ("WT_SESSION", "TERM_PROGRAM", "TERM"):
        value = os.getenv(name, "").strip()
        if value:
            hints.append(name if name == "WT_SESSION" else f"{name}={value}")
    return ", ".join(hints)
